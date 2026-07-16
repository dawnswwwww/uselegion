# Gap:Compaction 缺工程化防线(无熔断/复灌/PTL/cache)

> **实施状态**:Phase A/B/C/D 全部已实施,见 [`docs/DEVLOG.md`](../../../DEVLOG.md) 2026-07-09。遗留:ContextEngine 当前为 run-level 抽象,ingest/assemble/compact/after_turn 的完整生命周期接口待后续真正引入替代引擎时补齐。

| 字段 | 值 |
|---|---|
| 类目 | [03-shallow](./_index.md)(内核浅化) |
| 优先级 | P1(长会话稳定性) |
| 工作量 | M(1-2 人周) |
| 前置依赖 | 无(是 [session-resume](./session-resume.md) 的前置) |
| 关联 PRD | `agent-harness-prd.md` §6 R1/R6 |
| 关联分析 | `claude-code-analysis/analysis/04f-context-management.md`、`04-agent-memory.md` §11 |

---

## 1. 现状证据

legion 的 compaction **基础正确**,但缺 Claude Code 的工程化防线:

- **真实 token 计数**:`legion-runtime/src/token_counter.rs:8-11` 用 `tiktoken-rs::cl100k_base`,BPE 加载失败回退字符启发式。`estimate_message_tokens` 统计 content+role+tool_calls + 4 token 框架开销(`:18-41`)。
- **阈值触发 + summary**:`compaction.rs:55-62` 触发条件 = `context_window * threshold_ratio`;`compact_conversation`(`:72-108`)调 provider 生成 summary,保留 system + summary + 最近 N 条。
- **tool-use 不变量保护(亮点)**:`select_compaction_boundary`(`compaction.rs:115-158`)保证不拆开 tool_use/tool_result,有专门测试(`:246-277`)。
- **缺熔断**:无 `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`——连续 compact 失败会无限重试,浪费 API。
- **缺状态复灌**:compact 只产 summary,**不重新注入 skill/memory/工具能力/MCP/查看过的文件**——模型"醒来"后能力丢失。
- **缺 PTL 防御**:provider 返回 context overflow(prompt too long)时无自动剥头重试。
- **缺 prompt cache**:无 `cache_control`,summary 用同一 model_ref 而非独立(虽然 `config.rs:359` 注释提 summary subagent,未实现)。
- **缺预处理脱水**:image/document 不压缩直接进 compact,浪费 token。

**结论**:compaction 在 happy path 工作,但长会话下"失败雪崩 + 能力退化 + overflow 崩溃"三个问题未防。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:`Compactor` trait 可插拔;PRD R6 的 Context Engine 接口化(legacy 为默认实现)。
- **P2 安全**:compact 不丢失 tool-use 不变量(已有);边界标记持久化(供 resume)。
- **P3 增量**:新防线默认开,但可配关闭(关闭等价当前)。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:每次 compact 记录 token 前后、复灌项、熔断状态。
- **P6 失败显式**:熔断、PTL 重试、脱水各有明确行为。
- **P7 测试**:熔断触发、复灌完整、PTL 重试、不变量保护(已有)测试。

---

## 3. 架构设计

### 3.1 补齐 Claude Code 的五道防线(04f)

```
agent_loop 检测 token
   ▼ (1) 前置 buffer: token > window - AUTOCOMPACT_BUFFER(13000) 即触发
   ▼ (2) 预处理脱水: strip_images / strip_reinjected_attachments
   ▼ (3) compact: summary + 保留 keep_tokens 原文 + 不变量保护(已有)
   ▼ (4) 熔断检查: consecutive_failures >= 3 → 停止 auto-compact + 告警
   ▼ (5) 状态复灌: 重新注入 skill/memory/tools/MCP/viewed files + 写 boundary
```

### 3.2 状态复灌(关键)

借鉴 `04-agent-memory.md` §11.3 `createPostCompactFileAttachments` + Plan/Skill/工具能力重声明。legion compact 后:

```
CompactionResult.reattachments:
   - viewed_files(本会话查看过的文件路径,重新声明可读)
   - active_skills(当前激活的 skill body)
   - active_memory(召回的 top-N 记忆)
   - tool_manifest(工具/MCP 能力清单)
   - active_plan(若有 Coordinator 计划)
```

这些作为 compact 后的首批 messages 注入,模型"醒来"能力齐装。

### 3.3 熔断(借鉴 04f §3)

```
consecutive_compact_failures
   < 3  → 正常 compact
   >= 3 → 停止 auto-compact(避免每天 250K 次死锁 API),转 tracing::warn + 上报指标
   成功一次 → 计数清零
```

### 3.4 PTL 防御(借鉴 04-agent-memory §11.1)

provider 返回 `prompt_too_long` 错误时:
```
truncateHeadForPTLRetry: 一次剥 20% 最旧消息分组 → 重试
MAX_PTL_RETRIES(默认 3)次后 → 兜底强制 compact
```

### 3.5 Prompt Cache(因地制宜)

- **Anthropic**:`cache_control: { type: "ephemeral" }` 标记 system prompt + 历史 prefix 为 cache breakpoint,compact 后 prefix 变化需重新标记。
- **OpenAI**:自动 prompt caching(无需显式标记),保持 prefix 稳定即可。
- summary 用独立 cheap model(借 `config.rs:359` 注释的 summary subagent 思路)。

### 3.6 Context Engine 接口化(PRD R6)

```rust
#[async_trait]
pub trait ContextEngine: Send + Sync {
    async fn ingest(&self, msg: Message);              // 消息入上下文
    async fn assemble(&self) -> Vec<Message>;          // 组装发往 provider
    async fn compact(&self, cfg) -> Result<()>;        // 压缩
    async fn after_turn(&self);                        // turn 后处理(触发 auto_extract 等)
}
// LegacyContextEngine = 当前硬编码逻辑(默认实现)
```

---

## 4. 接口设计(Rust)

### 4.1 Compaction 配置与结果

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionConfig {
    pub threshold_ratio: f64,           // 默认 0.8
    pub buffer_tokens: usize,           // AUTOCOMPACT_BUFFER,默认 13000
    pub max_consecutive_failures: u8,   // 熔断,默认 3
    pub keep_tokens: usize,             // 保留原文,默认 20000
    pub strip_images: bool,             // 脱水,默认 true
    pub use_prompt_cache: bool,         // 默认 true(provider 支持时)
    pub summary_model: Option<String>,  // 独立 summary model,None=同主 model
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub summary: String,
    pub kept_messages: Vec<Message>,
    pub reattachments: Vec<Attachment>,      // 状态复灌
    pub boundary: BoundaryMark,              // 持久化标记(供 session-resume)
    pub cache_breakpoints: Vec<usize>,       // prompt cache 标记位置
    pub tokens_before: usize,
    pub tokens_after: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BoundaryMark {
    pub entry_index: usize,                  // transcript 中 compact 发生位置
    pub timestamp_iso: String,
    pub tokens_compacted: usize,
}
```

### 4.2 Compactor trait

```rust
#[async_trait]
pub trait Compactor: Send + Sync {
    async fn should_compact(&self, msgs: &[Message], cfg: &CompactionConfig) -> bool;
    async fn compact(&self, msgs: Vec<Message>, cfg: &CompactionConfig)
        -> Result<CompactionResult, CompactionError>;
}

pub struct CircuitBreaker {
    consecutive_failures: AtomicU8,
    max: u8,
}
impl CircuitBreaker {
    pub fn allow(&self) -> bool;          // < max
    pub fn record_success(&self);         // 清零
    pub fn record_failure(&self);         // +1
}
```

### 4.3 状态复灌

```rust
pub fn build_reattachments(ctx: &SessionContext) -> Vec<Attachment> {
    vec![
        Attachment::viewed_files(ctx.viewed_files.clone()),
        Attachment::active_skills(ctx.active_skills.clone()),
        Attachment::recalled_memory(ctx.recalled.clone()),
        Attachment::tool_manifest(ctx.tools.clone()),
    ]
}
```

### 4.4 PTL 重试

```rust
pub async fn with_ptl_retry<F, Fut>(provider_call: F, msgs: &mut Vec<Message>)
    -> Result<Response, ProviderError>
where F: Fn(Vec<Message>) -> Fut, Fut: Future<Output = Result<Response, ProviderError>>,
{
    for _ in 0..MAX_PTL_RETRIES {
        match provider_call(msgs.clone()).await {
            Ok(r) => return Ok(r),
            Err(ProviderError::PromptTooLong) => {
                truncate_head_20pct(msgs);   // 借鉴 truncateHeadForPTLRetry
            }
            Err(e) => return Err(e),
        }
    }
    // 兜底强制 compact
    Err(ProviderError::PromptTooLong)
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-runtime/src/compaction.rs:55-184` | 新增 buffer 前置触发、脱水、熔断(`CircuitBreaker`)、状态复灌、boundary 持久化。保留 tool-use 不变量保护。 |
| `legion-runtime/src/agent_loop.rs:114-222` | compact 后注入 `reattachments`;provider 调用包 `with_ptl_retry`。 |
| `legion-runtime/src/context.rs` | 复灌项的来源(viewed_files/skills/recalled/tools 状态)。 |
| `legion-provider` | Anthropic provider 支持 `cache_control` breakpoint;OpenAI 走自动 cache。 |
| `legion-runtime/src/context_engine.rs`(新) | `ContextEngine` trait + `LegacyContextEngine`(把现有 context.rs/compaction.rs 逻辑封装为默认实现,实现 PRD R6)。 |
| `legion-core/src/config.rs` | `compaction` 配置升级(新字段默认值兼容)。 |

---

## 6. 风险与权衡

### 6.1 熔断的副作用(借鉴 04f §3)
熔断后 auto-compact 停止,context 会继续增长直到 PTL。**缓解**:熔断不是永久,成功一次清零;且 PTL 防御兜底强制 compact。熔断主要防"provider 死锁式失败"的无限重试。

### 6.2 状态复灌的 token 成本
复灌项会增加 compact 后的初始 token。**缓解**:复灌项本身受各自上限(skill summary/memory top-N),且只复灌"激活"的(非全量)。

### 6.3 prompt cache 的 provider 差异
- Anthropic:显式 `cache_control`,需在 compact 后重新标记 prefix breakpoint。
- OpenAI:自动 cache,无需标记,但 prefix 必须稳定(compact 会破坏,属预期)。
- 通用 provider:不支持则 `use_prompt_cache` 降级为 noop。

### 6.4 summary model 独立化
当前 summary 用主 model(贵)。借 `config.rs:359` 注释思路,summary 走 cheap model 降本。代价:cheap model summary 质量略低;**缓解**:用中等 model(非最便宜)。

### 6.5 Context Engine 接口化的迁移成本
PRD R6 要求 ContextEngine 可插拔,当前逻辑硬编码。**取舍**:Phase A 先封装 `LegacyContextEngine`(行为不变),trait 化但只一个实现;真正多实现(如 Codex 式 context engine)留后续。

### 6.6 微压缩(reactiveCompact)
Claude Code 有实验性微压缩(轻量缩减)。legion **Phase B 不做**,因为完整 compact + 复灌已覆盖主场景;微压缩列为研究。

---

## 7. 实现路线图

### 阶段 A(Phase B,~0.5 人周):熔断 + buffer 前置 + 脱水
1. `CircuitBreaker` + `AUTOCOMPACT_BUFFER` 前置触发。
2. `strip_images`/`strip_reinjected_attachments` 脱水。
3. **验收**:连续 3 次 compact 失败后停止 auto-compact(熔断测试);image 被脱水。

### 阶段 B(Phase B,~0.5 人周):状态复灌 + boundary 持久化
1. `build_reattachments`(viewed_files/skills/recalled/tools)。
2. `BoundaryMark` 写入 transcript(供 session-resume)。
3. **验收**:compact 后模型仍知道激活的 skill/工具/记忆(复灌测试)。

### 阶段 C(Phase B,~0.5 人周):PTL 防御 + prompt cache + summary model
1. `with_ptl_retry`(剥 20% 重试 + 兜底 compact)。
2. Anthropic `cache_control` breakpoint;summary 独立 model。
3. **验收**:provider 返回 prompt_too_long 时自动剥头重试;Anthropic 请求带 cache breakpoint。

### 阶段 D(Phase B 尾,~0.5 人周):Context Engine trait 化
1. `ContextEngine` trait + `LegacyContextEngine`(封装现有逻辑)。
2. 配置 `context.engine: "legacy"`(默认)。
3. **验收**:legacy engine 行为等价当前(回归)。

---

## 8. 验收标准

- [x] 连续 3 次 compact 失败后停止 auto-compact(熔断测试);成功一次清零。
- [x] token 超 `window - buffer`(13000)即触发(前置测试)。
- [x] image/document 在 compact 前被脱水为 `[image]` 文本。
- [x] compact 后 `reattachments` 注入:激活 skill/recalled memory/tool manifest/viewed files(复灌测试)。
- [x] `BoundaryMark` 写入 transcript(供 [session-resume](./session-resume.md))。
- [x] provider 返回 `prompt_too_long` 时剥 20% 重试,3 次后返回错误(PTL 测试)。
- [x] Anthropic 请求带 `cache_control` breakpoint;summary 用独立 model。
- [x] tool-use 不变量保护不被破坏(回归已有测试)。
- [x] compact 记录 tokens_before/after + 复灌项 + 熔断状态(`tracing`)。
- [x] `ContextEngine` trait 化,legacy 实现行为不变。
- [x] `AGENTS.md` 更新 compaction 章节。

---

*上一篇:[`memory-layers.md`](./memory-layers.md) · 下一个 gap:[`session-resume.md`](./session-resume.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
