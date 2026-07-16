# Gap:Memory 单层 + 全手动(缺分层与自动决策)

| 字段 | 值 |
|---|---|
| 类目 | [03-shallow](./_index.md)(内核浅化) |
| 优先级 | P1(内核深度) |
| 工作量 | L(≥3 人周) |
| 前置依赖 | 无(与 [compaction](./compaction.md) 协同:session memory 可作 compact 断点) |
| 关联 PRD | `agent-harness-prd.md` §7 M2/M7/M8(分层/Active Memory/Dreaming) |
| 关联分析 | `claude-code-analysis/analysis/04-agent-memory.md` |

---

## 1. 现状证据

legion 的 memory 后端是**真货**(亮点),但能力停留在"单层 + 全手动":

- **后端真实可用**:`legion-memory/src/backend.rs:85-396` 三表(documents + FTS5 + vec0 虚表)+ **RRF 融合排序**(`:319,377`),`index_file`/`search`/`get`(行范围)完整,有测试。
- **单层存储**:只有 `documents` 表,分层仅是 `kind` 字符串标签(`legion-runtime/src/memory.rs:24`),**无独立检索策略**。
- **全手动写入**:记忆写入完全靠 agent 主动调用 `memory_index` 工具(`legion-tools/src/tools.rs:917-1004`)。**无自动记忆决策**——没有"这段对话是否值得记住"的判定,无 post-turn hook 写记忆。`grep "auto memory|should remember"` 零命中。
- **无召回选择器**:每次 `memory_search` 全量向量检索,无"过滤已召回/过滤近期工具"的去重。
- **PRD 未实现项**:M7 Active Memory(阻塞式记忆子代理)、M8 Dreaming(light/deep/REM)、M2 每日笔记(`memory/YYYY-MM-DD.md`)/DREAMS.md 均未实现。M9 备选后端(qmd/honcho/lancedb)未实现。

**结论**:memory 是"被动存储",缺 Claude Code 的"主动沉淀 + 分层检索 + 智能召回"。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:`MemoryBackend` trait 扩展自动决策与召回,可插拔后端。
- **P2 安全**:自动写入的记忆须过滤敏感信息(API key 等,借鉴 Team Memory 的 secret scanning);写入受 token 上限。
- **P3 增量**:自动决策默认关闭(`auto_extract: false`);关闭时等价当前手动行为。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:记忆写入/召回/衰减产生 `tracing` 事件。
- **P6 失败显式**:自动提取的 LLM 失败不影响主 turn(后台静默 + 告警)。
- **P7 测试**:分层检索、自动决策、召回去重各有测试。

---

## 3. 架构设计

### 3.1 借鉴 Claude Code 四层,因地制宜为三层 + SQLite

Claude Code 四层(Auto/Session/Agent/Team)基于文件目录。legion **已有 SQLite 单表 + `kind`**,更高效的做法:让 `kind` 驱动**不同检索权重与召回策略**,而非拆四套存储。

| 层(legion) | 对应 Claude Code | 语义 | 检索策略 |
|---|---|---|---|
| **Working**(短期) | Session Memory | 当前会话摘要,turn 间生效 | 高权重,自动随 compact 更新 |
| **Episodic**(事件) | Auto Memory | 跨会话的事件/对话事实 | 中权重,后台自动沉淀 |
| **Semantic**(知识) | Agent Memory | 持久知识/偏好/项目事实 | 低权重但高保留,衰减慢 |

Team Memory → 延后(P2,需多节点同步)。

### 3.2 自动记忆决策(Auto Extract)

借鉴 Claude Code `extractMemories` 后台流程 + 限制工具集(只读 + Edit/Write):

```
agent turn 结束
   ▼ (后台 tokio::spawn,不阻塞主 turn)
auto_extract(session_recent_msgs)
   ▼
轻量 LLM(走 cheap router)判断:是否有值得持久化的事实?
   ▼ 是
secret_scanning(过滤 API key/凭证) → index(kind=Episodic)
   ▼ 否
跳过
```

借鉴 `04-agent-memory.md` §5 的 extract memories prompt:**限制工具集**(Read/Grep/只读 Bash/Edit/Write)、只用最近若干消息、禁 MCP/Agent。

### 3.3 召回选择器(Relevant Memory Recall)

借鉴 `findRelevantMemories()`:不全量向量检索,而是:

```
recall(query, ctx)
   ▼
1. 扫描 memory manifest(轻量,类似 Claude 的 memory 文件头)
   ▼
2. 轻量 LLM 选择器选 top-N(默认 5)
   ▼
3. 过滤 already_surfaced(避免重复召回)
   ▼
4. 过滤 recent_tools(避免重复活跃工具文档)
   ▼
返回 top-N 注入 prompt
```

Phase A 的选择器用关键词匹配(零成本);Phase B 接轻量 LLM。

### 3.4 记忆衰减与合并
- Episodic 记忆有 `last_accessed` + `access_count`;长期未访问降权。
- 语义相似的 Episodic 记忆定期合并(避免碎片)。
- 借鉴 `04-agent-memory.md` §11 的 compaction 联动:Working 层 memory 可直接作 compact 断点。

---

## 4. 接口设计(Rust)

### 4.1 扩展 `MemoryBackend` trait(`legion-memory`)

```rust
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind { Working, Episodic, Semantic }

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    // 现有
    async fn search(&self, query: &str, opts: SearchOpts) -> Result<Vec<MemoryHit>, MemoryError>;
    async fn get(&self, id: &str, range: Option<LineRange>) -> Result<String, MemoryError>;
    async fn index(&self, content: &str, meta: MemoryMeta) -> Result<String, MemoryError>;

    // 新增:自动决策
    async fn auto_extract(&self, session: &SessionSnapshot) -> Result<ExtractReport, MemoryError>;

    // 新增:召回选择器
    async fn recall(&self, query: &str, ctx: &RecallContext) -> Result<Vec<MemoryHit>, MemoryError>;

    // 新增:衰减/合并
    async fn decay_and_merge(&self) -> Result<DecayReport, MemoryError>;
}

pub struct MemoryMeta {
    pub kind: MemoryKind,
    pub source: String,            // agent_id / session_key
    pub tags: Vec<String>,
    pub confidence: f32,
}

pub struct RecallContext {
    pub already_surfaced: std::collections::HashSet<String>,  // 过滤已召回
    pub recent_tools: Vec<String>,                             // 过滤近期工具文档
    pub limit: usize,                                          // 默认 5
    pub prefer_kinds: Vec<MemoryKind>,                         // 权重偏好
}
```

### 4.2 自动决策实现

```rust
pub struct AutoExtractor {
    router: Arc<dyn ProviderRouter>,   // 走 cheap model
    summarizer_model: ModelRef,        // 如 "cheap-router/extract"
    secret_scanner: SecretScanner,
}

impl AutoExtractor {
    pub async fn extract(&self, msgs: &[Message]) -> Result<Vec<MemoryFact>> {
        // 1. 限制:只用最近 N 条消息
        // 2. prompt:提取 durable facts(借鉴 extract memories prompt)
        // 3. secret_scanner.filter(facts)  // 过滤凭证
        // 4. 返回 facts → 交 backend.index(kind=Episodic)
    }
}
```

### 4.3 配置 schema(`legion-core`)

```jsonc
// legion.json
{
  "memory": {
    "backend": "builtin",
    "autoExtract": {
      "enabled": true,
      "model": "cheap-router/extract",
      "maxMessages": 20,
      "cooldownSeconds": 300
    },
    "recall": {
      "limit": 5,
      "useLlmSelector": false
    },
    "decay": {
      "enabled": true,
      "episodicMaxAge": "90d"
    }
  }
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-memory/src/backend.rs` | `documents` 表已有;`MemoryBackend` trait 增 `auto_extract`/`recall`/`decay_and_merge`;`MemoryKind` 驱动检索权重。 |
| `legion-runtime/src/memory.rs:24` | `kind` 字段从字符串升级为 `MemoryKind` 枚举;分层检索策略。 |
| `legion-runtime/src/agent_loop.rs` | turn 结束后 `tokio::spawn` 后台 `auto_extract`(不阻塞);轮首 `recall` 注入 top-N。 |
| `legion-runtime/src/context.rs:98-124` | 现有 MEMORY.md 注入 + 检索逻辑改为走 `recall`(带去重)。 |
| `legion-provider` | 复用 router 走 cheap model 做 extract/selector。 |
| `legion-core/src/config.rs` | `memory` 配置升级(autoExtract/recall/decay,默认 autoExtract.enabled=false 兼容)。 |

---

## 6. 风险与权衡

### 6.1 分层:文件目录 vs SQLite kind(因地制宜)
Claude Code 用目录区分层(每层独立检索)。legion 已有 SQLite,**用 `kind` 字段 + 检索权重**更高效(避免四套索引)。代价:层间边界不如文件清晰;**缓解**:严格按 `kind` 应用不同保留策略(Working 随 compact 清理,Semantic 长期保留)。

### 6.2 自动决策的成本与噪声
后台 LLM 提取记忆有 token 成本,且可能产生低质记忆。**缓解**:
- 限 `maxMessages`(只看最近 N 条);
- `cooldownSeconds` 防频繁触发;
- `confidence` 阈值过滤;
- `decay_and_merge` 定期清理碎片。

### 6.3 secret scanning(安全)
自动提取的记忆可能误存 API key/凭证。借鉴 Claude Code Team Memory 的 secret scanning:`AutoExtractor` 写入前扫描,命中则 redact 或丢弃。

### 6.4 召回去重(借鉴 04-agent-memory §5)
`already_surfaced` 与 `recent_tools` 过滤,避免:① 同一记忆每轮重复注入;② 已在 prompt 的工具文档重复召回。

### 6.5 因地制宜:后台进程 vs fork
Claude Code 用 sandboxed forked subagent 维护 session memory。legion 用 `tokio::spawn` 后台任务 + 限制工具集(不 fork 进程),更轻量。

### 6.6 Dreaming 延后
PRD M8 Dreaming(light/deep/REM 阶段晋升)是创新但复杂,且 ROI 不明确。列为 Phase C 研究,不进 Phase A/B。

---

## 7. 实现路线图

### 阶段 A(Phase B,~1.5 人周):分层检索 + 召回去重 ✅ 已实施(2026-07-10)
1. `MemoryKind` 枚举 + 检索权重(Working 1.0 / Episodic 0.75 / Semantic 0.55);`recall` + `RecallContext`(already_surfaced/recent_tools 过滤)。✅
2. `context.rs` 改走 `recall`,`recent_tools` 取自 `tool_registry.definitions()`。✅
3. **验收**:同记忆不重复注入;近期工具文档不重复召回(单测 + context 集成测试覆盖)。✅
   - 偏离:`recent_tools` 采用 id 等值匹配(非 `content.contains`),避免 `read`/`exec` 误伤正文。
   - 已随 Phase C 落地:可配 `limit`(`memory.recall.limit`)、跨 turn 持久化 `already_surfaced`(`SurfacedStore`)、轻量 LLM 召回选择器(`LlmRecallSelector`)。

### 阶段 B(Phase B,~1.5 人周):自动决策 ✅ 已实施(2026-07-10)
1. `AutoExtractor`(cheap router + extract prompt + secret scanning)。✅
2. `agent_loop` turn 后后台触发(`run_loop` 工具循环结束后 `tokio::spawn`);cooldown + maxMessages 限制。✅
3. `memory.autoExtract` 配置(默认 false,开启才生效)。✅
4. **验收**:开启 autoExtract 后,对话中的事实被自动沉淀;凭证被过滤(secret_scanner + auto_extract + agent_loop 集成测试)。✅
   - 决策:secret 命中=丢弃整条事实(drop),不 redact;抽取失败全程 `tracing::warn` 吞掉,不影响主 turn。

### 阶段 C(Phase B 尾,~0.5 人周):衰减合并 + 轻量 LLM 召回 ✅ 已实施(2026-07-10)
1. `decay_and_merge`(episodic 老化降权 + 相似合并):查询时按 `created_at` 对 episodic 乘 `decay_factor`(半衰期 `halfLifeDays`,默认关);合并按 keep-newest 确定性分组删除,经 `legion memory merge` 触发。✅
2. 召回选择器可选接轻量 LLM(`memory.recall.useLlmSelector: true` + `selectorModel`):`LlmRecallSelector` 镜像 `LlmSkillSelector`,失败回退原顺序。✅
3. 可配 `memory.recall.limit`(默认 5)+ 跨 turn 持久化 `SurfacedStore`(`~/.legion/agents/<agent>/surfaced/<hash>.json`),同一会话已注入的事实不再重复注入。✅
4. **验收**:90 天未访问的 episodic 记忆降权(开启 `decay.enabled`);LLM 召回重排生效;同一会话第二轮不重复注入同一事实(单测 + agent_loop 集成测试 + backend 集成测试覆盖)。✅
   - 决策:合并=确定性 keep-newest(LLM 摘要合成留后续);衰减仅作用于 episodic 且默认关;LLM 选择器只作用于每轮注入路径,compaction 仍走关键词 recall。

### 阶段 D(Phase C,研究):Team Memory / Dreaming
- 多节点记忆同步(pull/push/checksum);Dreaming 阶段晋升。暂不承诺。

---

## 8. 验收标准

- [x] `MemoryKind`(Working/Episodic/Semantic)驱动不同检索权重(Phase A,2026-07-10)。
- [x] `recall` 过滤 `already_surfaced` + `recent_tools`(去重测试,Phase A;跨 turn 持久化 `SurfacedStore` 已由 Phase C 落地)。
- [x] `autoExtract.enabled=true` 时,turn 后后台提取事实并写入 Episodic 层(Phase B,2026-07-10)。
- [x] 自动提取过滤凭证(secret scanning 测试:API key 不入库;命中即丢弃整条事实)。
- [x] 自动提取 LLM 失败不影响主 turn(后台静默 + tracing 告警;集成测试覆盖)。
- [x] `autoExtract.enabled=false` 时行为等价当前手动 memory(默认不 spawn,回归安全)。
- [x] `decay_and_merge` 降低 90 天未访问 episodic 权重(查询时 `decay_factor` + CLI `legion memory merge` 合并,Phase C)。
- [x] 召回 top-N 默认 5(经 `RecallContext.limit` 可配;`memory.recall.limit` 配置项已由 Phase C 落地)。
- [x] 记忆写入/召回/衰减有 `tracing` 事件(`surfaced` 写盘失败 `warn`、合并/选择器缺 model `warn`、auto-extract 静默 `warn`)。
- [x] `AGENTS.md` 更新 memory 章节(声明分层、自动决策与 Phase C 召回/衰减/合并,Phase A+B+C)。

---

*上一篇:[`sandbox-isolation.md`](./sandbox-isolation.md) · 下一个 gap:[`compaction.md`](./compaction.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
