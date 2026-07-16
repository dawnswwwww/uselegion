# Gap:Prompt 管理固定拼装(无分层/override 优先级/可观测)

| 字段 | 值 |
|---|---|
| 类目 | [03-shallow](./_index.md)(内核浅化) |
| 优先级 | P1(是 skills/memory 注入的载体) |
| 工作量 | M(1-2 人周) |
| 前置依赖 | [skills](../02-missing/skills.md)(skill 注入需要 section 化的 prompt) |
| 关联 PRD | `agent-harness-prd.md` §6 R3(系统提示分层) |
| 关联分析 | `claude-code-analysis/analysis/04g-prompt-management.md` |

---

## 1. 现状证据

legion 的 prompt 组装是**单一拼接函数**,缺分层与可观测:

- **固定拼装**:`legion-runtime/src/context.rs:81-131` 的 `assemble_system_prompt` 顺序拼接:Base sections → MEMORY.md → 记忆检索 → override。
- **bootstrap 不全**:`context.rs:78` 的 `BOOTSTRAP_FILES` 仅 4 个(AGENTS/SOUL/USER/TOOLS.md)。PRD R2 的 HEARTBEAT/IDENTITY/MEMORY(独立)/BOOTSTRAP.md 未作为 bootstrap 注入(MEMORY.md 单独处理)。
- **无 section 模型**:prompt 是一整个字符串,无法按段统计 token、无法按段替换/覆盖、无法按段决定是否参与 prompt cache。
- **无 override 优先级链**:没有 `override > coordinator > agent > custom > default` 的解析;现有 `override` 是简单字符串覆盖。
- **无 custom 替换语义**:PRD/agent 配置若给 customSystemPrompt,语义不明确(替换 default?append?)。Claude Code 明确:custom **直接替代** default(不 append),appendSystemPrompt 挂末尾。
- **无可观测**:无 `dump-prompts`(落 JSONL 供调试)、无按 section 统计 token 的 `/context` 等价物。

**结论**:prompt 组装能工作,但随着 skills/memory/MCP/multi-agent 注入增多,**不可观测、不可控、易爆 token**,且 custom prompt 语义模糊。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:`SystemPromptBuilder` section 化,各子系统(skills/memory/MCP/output_style)以 section 注册。
- **P2 安全**:custom prompt 来源受控(仅 agent 配置/CLI,不来自用户消息);section 有 token 上限防爆。
- **P3 增量**:无 custom/override 时,build 结果等价当前字符串。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:`dump` 落 JSONL + 按 section token 报告(借鉴 `/context`)。
- **P6 失败显式**:section 超 token 上限截断并告警;custom 解析失败明确报错。
- **P7 测试**:override 优先级、custom 替换语义、token 截断、dump 各有测试。

---

## 3. 架构设计

### 3.1 section 化(借鉴 04g §3,因地制宜)

Claude Code `getSystemPrompt()` 返回字符串数组(静态主干 + 动态 section,每段独立缓存/计 token)。legion 用 `PromptSection` 模型化,但**不强调缓存**(Rust 无 JS 对象重建开销),而是强调:

```
SystemPromptBuilder
   sections: Vec<PromptSection>
       │
       ▼ build()
   resolve_sections() 按优先级合并:
       Override > Coordinator > Agent > Custom > Default
       (custom 替换 default 同 id 段;append 挂末尾)
       │
       ▼
   最终 system prompt 字符串 + token 报告 + cache breakpoint 位置
```

### 3.2 section 清单(legion 化)

| SectionId | 来源 | 内容 |
|---|---|---|
| `Base` | Default | 主 system prompt(核心行为指令) |
| `Agents` | Default | AGENTS.md bootstrap |
| `Soul` / `User` / `Tools` | Default | SOUL/USER/TOOLS.md bootstrap |
| `Identity` / `Heartbeat` | Default | (新增)IDENTITY/HEARTBEAT.md |
| `Memory` | Default | MEMORY.md 索引 + recalled top-N |
| `Skills` | Default/Agent | 激活 skill 摘要/body |
| `EnvInfo` | Default | 环境/日期/gitStatus |
| `Language` / `OutputStyle` | Default/Override | 语言/输出风格 |
| `McpInstructions` | Default | MCP 工具说明 |
| `Custom` | Custom | agent 配置的 customSystemPrompt(替换 Base) |
| `Append` | Append | appendSystemPrompt |

### 3.3 override 优先级(借鉴 04g §4 `buildEffectiveSystemPrompt`)

```
解析顺序(高 → 低):
   1. Override        — 强制覆盖某 section(coordinator/skill 注入)
   2. Coordinator     — multi-agent coordinator 身份重写
   3. Agent           — agent 配置的 section
   4. Custom          — customSystemPrompt(替换 Base,不 append)
   5. Default         — 内建 section
   末尾: Append       — appendSystemPrompt 永远挂末尾
```

### 3.4 可观测(借鉴 04g §10)

- `dump-prompts`:每次 build 落 JSONL 到 `~/.legion/dump-prompts/<session>.jsonl`,含每个 section 的 content + tokens。
- token 报告:`legion context` CLI(或 `agent` 子命令)按 section 列 token 占用。

### 3.5 专项 prompt 协议化(借鉴 04g §9)

compact/session-memory/extract-memories 等后台任务用独立、受限的 prompt(已在 [compaction](./compaction.md)/[memory-layers](./memory-layers.md) 引用)。本 gap 统一这些专项 prompt 的管理。

---

## 4. 接口设计(Rust)

### 4.1 PromptSection 与 Builder(`legion-runtime/src/context.rs` 重构)

```rust
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SectionId {
    Base, Agents, Soul, User, Tools, Identity, Heartbeat,
    Memory, Skills, EnvInfo, Language, OutputStyle, McpInstructions,
    Custom, Append, Other(String),
}

#[derive(Debug, Clone)]
pub enum SectionSource { Default, Coordinator, Agent(String), Custom, Override, Append }

#[derive(Debug, Clone)]
pub struct PromptSection {
    pub id: SectionId,
    pub content: String,
    pub source: SectionSource,
    pub cacheable: bool,        // 是否参与 prompt cache(默认 true)
    pub max_tokens: Option<usize>,  // 超限截断
}

pub struct SystemPromptBuilder {
    sections: Vec<PromptSection>,
}

impl SystemPromptBuilder {
    pub fn new() -> Self;
    pub fn add(&mut self, section: PromptSection) -> &mut Self;
    /// 按 source 优先级合并,返回最终 prompt + token 报告 + cache breakpoints。
    pub fn build(&self) -> BuiltPrompt;
    /// 落 dump JSONL(可观测)。
    pub fn dump(&self, dest: &Path) -> std::io::Result<()>;
}

pub struct BuiltPrompt {
    pub text: String,
    pub section_tokens: Vec<(SectionId, usize)>,  // 按 section token 报告
    pub total_tokens: usize,
    pub cache_breakpoints: Vec<usize>,            // prompt cache 标记位置
    pub truncated: Vec<SectionId>,                // 被截断的 section
}

/// override 优先级解析。
pub fn resolve_sections(sections: &[PromptSection]) -> Vec<PromptSection> {
    // 1. 按 SectionId 分组
    // 2. 同 id 内按 source 优先级选(Override > Coordinator > Agent > Custom > Default)
    // 3. Custom 替换 Base(同 id)
    // 4. Append 段单独挂末尾
}
```

### 4.2 配置 schema(`legion-core`,per-agent)

```jsonc
{
  "agents": {
    "list": [{
      "id": "researcher",
      "customSystemPrompt": "...",     // 替换 Base(不 append)
      "appendSystemPrompt": "...",     // 挂末尾
      "outputStyle": "concise",
      "language": "zh-CN"
    }]
  }
}
```

```rust
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentPromptConfig {
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub output_style: Option<String>,
    pub language: Option<String>,
}
```

### 4.3 CLI

```bash
legion agent --dump-prompts            # 本 turn 落 dump JSONL
legion context <session>               # 列 system prompt 各 section token 占用
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-runtime/src/context.rs:78-131` | `assemble_system_prompt` 重构为 `SystemPromptBuilder`;bootstrap 补 HEARTBEAT/IDENTITY;MEMORY/Skills/MCP 作为 section 注册。 |
| `legion-runtime/src/agent_loop.rs` | build 后取 `cache_breakpoints` 传 provider(配合 [compaction](./compaction.md) prompt cache)。 |
| `legion-core/src/config.rs` | per-agent `customSystemPrompt`/`appendSystemPrompt`/`outputStyle`/`language`。 |
| `legion-cli` | `--dump-prompts` flag;`legion context` 子命令(读 dump + 列 token)。 |
| `legion-provider` | Anthropic provider 用 `cache_breakpoints` 插 `cache_control`。 |

---

## 6. 风险与权衡

### 6.1 section 缓存的意义(因地制宜)
Claude Code section 缓存省 JS 对象重建。legion 是 Rust,**重建开销可忽略**,故不强调缓存;但仍保留 `cacheable` 标记——用于 **prompt cache**(provider 层)的 breakpoint 决策,与 compaction 协同。

### 6.2 custom 替换 vs append(语义明确化)
借鉴 04g §4:`customSystemPrompt` **替换 Base**(不 append),`appendSystemPrompt` 永远挂末尾。这消除当前 override 语义模糊。**风险**:用户可能期望 append;**缓解**:文档明确 + config 校验时若两者都给则告警。

### 6.3 token 上限与截断
section 累积可能爆 context。**缓解**:每个 section 可设 `max_tokens`,超限截断(借鉴 Claude `truncateEntrypointContent`);`BuiltPrompt.truncated` 报告被截段;Skills/Memory 摘要本就有上限(见各自 gap)。

### 6.4 dump 的隐私
dump JSONL 含 system prompt 全文(可能含注入的 memory/user 上下文)。**缓解**:dump 默认关(`--dump-prompts` 显式触发);文件权限 0600;不含 API key(凭证不入 prompt)。

### 6.5 专项 prompt 一致性
compact/session-memory/extract-memories 的 prompt 散落各处。本 gap 统一到一个 `prompts/` 模块或资源文件,便于维护与本地化。

### 6.6 prompt cache 与 section 变动
动态 section(EnvInfo 含日期、recalled memory 每轮变)会破坏 prompt cache。借鉴 Claude `DANGEROUS_uncachedSystemPromptSection`:动态段标记为不参与 cache 前缀,放在 cache breakpoint 之后。

---

## 7. 实现路线图

### 阶段 A(Phase B,~0.5 人周):section 化重构 + bootstrap 补全 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `PromptSection`/`SystemPromptBuilder`/`BuiltPrompt`(新建 `legion-runtime/src/prompt.rs`):`SectionId` 全覆盖 gap §3.2 清单 + 细分(`RelevantMemories`/`MemoryTools`/`SkillsSummary`/`SkillsBody`/`RunOverride`),`SectionSource` 枚举就位(阶段 B 优先级链用);现有 assemble 逻辑迁移为 section 注册。✅
2. bootstrap 补 `IDENTITY.md`/`HEARTBEAT.md`(`BOOTSTRAP_FILES` 6 项,缺文件跳过)。✅
3. 无 custom/override 时 build 与旧字符串**逐字一致**(`assemble_system_prompt_report(...).text == assemble_system_prompt(...)` 回归断言;全部既有 context 测试未改即过)。✅
4. `max_tokens` 截断(line-wise + 显式 marker + `BuiltPrompt.truncated` 报告)与 `section_tokens`/`total_tokens` 报告已随 builder 落地。✅
5. **验收**:重构后 system prompt 内容与当前一致(回归);新增 bootstrap 生效(`identity_and_heartbeat_bootstrap_files_are_loaded`);report 暴露按段 token(`report_exposes_per_section_tokens`)。✅

> 微调:阶段 A 的 `build()` 保持注册序拼接、**不做同 id 去重**;override 优先级合并(`resolve_sections`)与 custom/append 语义留给阶段 B。截断/报告属 gap §7 阶段 C 范畴,因 builder 天然支持而提前落地,CLI/dump 暴露仍待阶段 C。

### 阶段 B(Phase B,~0.5 人周):override 优先级 + custom/append 语义 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `resolve_sections` 优先级链(`source_rank`:Override5 > Coordinator4 > Agent3 > Custom2 > Default1;同 id 取最高、平局留先注册者;`Append` source 全部保留并移至末尾;不同 id 按首注册序)。✅
2. per-agent `customSystemPrompt`/`appendSystemPrompt`/`outputStyle`/`language` 配置(`AgentConfig` 4 字段,`assemble_system_prompt_report` 第 9 参接线,`agent_loop` 按 `request.agent_id` 查表注入)。✅
3. **验收**:custom 替换 Base 测试(`custom_base_replaces_default_base_section`/`resolve_custom_replaces_default_same_id`);append 挂末尾测试(`resolve_append_sections_move_to_end_in_order`);优先级覆盖测试(`resolve_highest_rank_wins_per_id`/`resolve_agent_source_beats_custom_but_loses_to_coordinator`);agent 配置注入测试(`agent_prompt_overrides_register_sections`)。✅

### 阶段 C(Phase B 尾,~0.5 人周):可观测 + token 上限 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `BuiltPrompt::write_dump` 落 JSONL 到 `~/.legion/dump-prompts/<session>.jsonl`(append、0600 权限、每行含 ts/session/sections[{id,source,tokens,truncated}]/total_tokens/cache_prefix_len);`legion context <session>` 读最后一行列按段 token 表;`legion agent --dump-prompts` flag 经 WS `dumpPrompts` 参数 → `RunRequest::with_dump_prompts`;顶层 `promptDump.enabled` 配置全局开启。✅
2. section `max_tokens` 截断 + `truncated` 报告(阶段 A 已随 builder 落地)。✅
3. `cacheable` 标记 → `BuiltPrompt.cache_prefix_len`(前导连续 cacheable 段的字节累计,遇 uncached 即止;provider 多 system block 接线留后续)。✅
4. **验收**:`write_dump_appends_jsonl_record`(JSONL 内容 + append + 0600);`cache_prefix_stops_at_first_uncached_section`/`cache_prefix_covers_all_cacheable_sections`;CLI `render_dump_record_formats_section_table`/`latest_dump_record_reads_last_line`。✅

---

## 8. 验收标准

- [x] system prompt 由 `SystemPromptBuilder` section 化组装,非单一拼接。(Phase A)
- [x] bootstrap 含 HEARTBEAT/IDENTITY(补全 PRD R2)。(Phase A)
- [x] `customSystemPrompt` 替换 Base(不 append);`appendSystemPrompt` 挂末尾(语义测试)。(Phase B)
- [x] override 优先级:Override > Coordinator > Agent > Custom > Default(优先级测试)。(Phase B)
- [x] `legion context` 列各 section token 占用;`--dump-prompts` 落 JSONL(权限 0600)。(Phase C)
- [x] section 超 `max_tokens` 截断,`truncated` 报告。(Phase A,builder 层;Phase C 随 dump 暴露)
- [x] 动态 section(EnvInfo/recalled memory)不破坏 prompt cache prefix(cacheable 标记测试)。(Phase C:`cache_prefix_len` 已落地;provider breakpoint 接线留后续)
- [x] 无 custom/override 时 build 等价当前(回归)。(Phase A)
- [x] `AGENTS.md` 更新 prompt 章节(声明 section 模型与 override 语义)。(Phase A/B/C)

---

*上一篇:[`session-resume.md`](./session-resume.md) · 返回类目:[`_index.md`](./_index.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
