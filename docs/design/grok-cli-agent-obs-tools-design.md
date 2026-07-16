# Grok CLI → Legion：Agent 运行时 / 可观测性 / 工具 功能点级 diff 与设计方案

> 本文在 [`docs/design/gaps/05-grok-cli-comparison.md`](./gaps/05-grok-cli-comparison.md) 的高层差距基础上，针对 **Agent 运行时**、**可观测性**、**工具** 三个领域做功能点级拆解，并给出可在 Legion 落地的详细设计方案。
> 所有结论以两个仓库的源码为准；引用路径均为相对根目录。

---

## 1. 方法论：如何阅读本文

- **Diff 列**：`Grok 有 / Legion 无` 或 `Grok 深 / Legion 浅` 的功能节点。
- **设计列**：给出 Legion 侧可落地的最小可行改动（MVC）到完整形态的路径。
- **源码锚点**：关键类型/函数/文件位置，便于后续编码时直接定位。
- **优先级**：`P0`（安全/架构地基）、`P1`（体验或能力杠杆）、`P2`（体验增强）。

---

## 2. Agent 运行时

### 2.1 核心抽象对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **Agent 构造** | `AgentBuilder` 从 `AgentDefinition` + `PromptContext` + `ToolBridge` 构建不可变 `Agent`（`xai-grok-agent/src/{agent.rs,builder.rs}`） | 只有 `AgentRuntime` + `RunRequest`，没有独立的 `Agent` 类型（`crates/legion-runtime/src/{agent_loop.rs,types.rs}`） | Legion 的 agent 概念被淹没在 runtime 中 |
| **Agent 定义文件** | `.grok/agents/*.md` frontmatter 定义 name/description/tools/persona/permission_mode 等（`xai-grok-agent/src/config.rs`） | `config.agents.list` 仅配置 model/id/system_prompt（`crates/legion-core/src/config.rs`） | 缺少结构化 agent 定义与工具集 preset |
| **工具桥** | `ToolBridge` 持有 `ToolRegistry` + `ToolState` + `SessionContext`（`xai-grok-tools/src/bridge/`） | `CoreToolRegistry` 直接注入 `AgentRuntime` | 缺少会话级工具状态桥 |
| **Hosted tools** | `hosted_tools: Vec<HostedTool>` 用于服务端原生工具（如 WebSearch），请求时作为原生 Responses API 类型发送（`xai-grok-agent/src/agent.rs:46`） | 无 | 无法让 provider 在服务端执行搜索 |
| **PromptContext** | 封装 system prompt 渲染、AGENTS.md、personas、skills、audience（Primary/Subagent）（`xai-grok-agent/src/prompt/context.rs`） | `SystemPromptBuilder` 已 section 化（memory-layers Phase C），但缺少 audience/audience-aware 注入 | 部分具备，需扩展 audience 和 personas |
| **Agent 模式** | `Tui` / `Headless` / `Stdio` / `Serve` / `Leader` / `Generic`（`xai-grok-shell/src/agent/config.rs`） | 仅通过 `Harness` 区分 built-in / `acp:` 前缀外部 harness | 缺少运行形态抽象 |

### 2.2 Turn loop 与调度对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **循环位置** | `SessionActor` 在 `xai-grok-shell/src/session/` 中驱动 ACP 消息、采样、工具调用、compaction | `AgentRuntime::run` + `agent_loop.rs` 驱动 provider chat + tool pipeline | 两者结构类似，Grok 的循环更厚重 |
| **最大迭代** | 由 `AgentDefinition` / sampling config 控制 | `DEFAULT_MAX_ITERATIONS = 10`，`config.agents.defaults.max_iterations` | 相当 |
| **后台任务** | Bash tool 支持 `is_background`，配合 `TaskOutputTool`/`WaitTasksTool`/`KillTaskTool` 管理（`xai-grok-tools/src/implementations/grok_build/bash/`） | `exec` 工具同步执行，无后台任务管理工具 | Legion 无法让 agent 启动长时间运行任务后继续 |
| **Goal 编排** | 完整 goal_* 模块：planner、strategist、summarizer、stop detector、classifier、tracker、orchestrator（`xai-grok-shell/src/session/goal_*.rs`） | 无 | Legion 没有 agent 自我规划/目标跟踪层 |
| **Plan mode** | `PlanModeTracker` 状态机（Inactive/Pending/Active/ExitPending），持久化到 `plan_mode.json`，`enter_plan_mode`/`exit_plan_mode` tools（`xai-grok-shell/src/session/plan_mode.rs`） | 只有声明式 `FlowRunner`（Task Flow DAG），无交互式 plan mode | 缺少面向用户的计划模式 |
| **Scheduler** | `scheduler_create`/`delete`/`list` tools + `/loop` 斜杠命令（`xai-grok-tools/src/implementations/grok_build/scheduler/`） | `legion-automation` 有 cron + TaskRunner，但无 agent 可调用 scheduler tools | agent 无法创建周期性任务 |
| **Turn-end gating** | `TodoGate` + `CompletionRequirement` 检查（`xai-grok-shell/src/session/turn_completion.rs`） | `todo.rs` 有 todo 列表，但无 turn-end gate | 缺少轮次结束时的完成条件校验 |

### 2.3 Compaction 对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **触发阈值** | `auto_compact_threshold_percent` + `GROK_PREFIRE_LEAD_PERCENT`（默认 10pp 预触发） | `threshold_ratio` 或 `buffer_tokens` | Grok 预触发可避免窗口突然打满 |
| **算法** | 双阶段 compaction：pass1 总结 95% 历史 → NOTE₁；pass2 重写 NOTE₁ + 5% tail → NOTE₂（`xai-grok-shell/src/session/two_pass.rs`） | 单阶段总结旧消息，保留最近 `min_messages_to_keep`（`crates/legion-runtime/src/compaction.rs`） | Legion compaction 较简单，缺少双阶段 |
| **状态持久化** | `CompactOutput`、checkpoint persistence、segments mode | `CompactionResult` 含 `boundary`，`RunEvent::Compaction` 携带 `resume_head` | Legion 已支持 resume boundary；Grok 的 checkpoint/segments 更细 |
| **熔断** | 无显式 circuit breaker，但 prefire 失败有 `PrefireOutcome` 分类 | `CircuitBreaker` 在连续失败 `max_consecutive_failures` 后断开 | Legion 已有基础熔断 |
| **Reattachments** | 由 Agent/PromptContext 重新注入 skills、viewed files、memory | `Reattachment` enum（ViewedFiles/ActiveSkills/RecalledMemory/ToolManifest） | 两者都具备，Grok 的注入与 AgentBuilder 更耦合 |

### 2.4 权限与审批对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **权限模式** | `default` / `acceptEdits` / `auto` / `dontAsk` / `bypassPermissions` / `plan`（`xai-grok-agent/src/config.rs`） | `Approval::Off` / `Prompt` / `Required`（`crates/legion-runtime/src/approval.rs`） | Legion 权限模式单一 |
| **审批流程** | Permission rules + remembered grants + hooks + built-in auto-approvals + prompt policy（多层检查） | `can_use_tool: CanUseToolFn` 返回 `Permission::Allow/Prompt/Deny`，`ApprovalGate` 等待用户回复 | Legion 审批回路是 Part 1，缺 rules/remembered grants/hooks |
| **Yolo** | `--yolo` 自动批准；`bypassPermissions` 模式 | `--yolo` 通过 `ApprovalGate::with_auto_approve` | 相当 |
| **Unattended** | `dontAsk` 模式拒绝无显式允许的操作 | `interactive == false` 时 `Permission::Prompt` 失败关闭 | 相当 |

### 2.5 Session 状态对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **持久化** | `~/.grok/sessions/` JSONL chat history、`summary.json`、`signals.json`、background task manifest、`plan_mode.json` | `SessionStore` 在内存，`transcript_repair.rs` + TTL/archive | Legion 的持久化偏 runtime 内部 |
| **Resume** | `--resume [SESSION_ID]`、`--continue`、`--fork-session`、跨 worktree 解析、leader 重连回放 | `load_for_resume` + orphan repair + lite read | Legion 恢复机制有，但缺少用户级 fork/rewind/leader 回放 |
| **崩溃恢复** | Leader 模式自动更新 + 会话重放 | orphan repair + consistency check | 方向不同 |

---

## 3. 可观测性

### 3.1 架构对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **产品遥测** | `TelemetryClient` 统一路由到 product events + Mixpanel（`xai-grok-telemetry/src/client.rs`） | 无产品遥测客户端 | 完全缺失 |
| **统一日志** | `~/.grok/logs/unified.jsonl`，5 MB 轮替，shell 直接写，pager/desktop 通过 ACP `x.ai/log` 转发（`xai-grok-telemetry/src/unified_log.rs`） | tracing + logging，无统一 JSONL 日志 | 完全缺失 |
| **Session metrics** | `session_started`、`turn`、`turn_completed`、`doom_loop_recovery`、trace upload 事件（`xai-grok-telemetry/src/session_metrics.rs`） | `RunEvent` 流包含 Lifecycle/ToolStart/ToolEnd/Compaction/TodoUpdate，但无 session lifecycle 事件 | 事件粒度不同，缺 lifecycle 事件 |
| **崩溃报告** | Sentry 集成（`xai-grok-telemetry/src/sentry.rs`） | 无 | 缺失 |
| **分布式追踪** | 内部 OTLP trace pipeline + 外部 OTEL stream（`xai-grok-telemetry/src/otel_layer/`） | 仅 `tracing` 日志，无 OTLP exporter | 缺失 |
| **指标** | Mixpanel + product events；无 Prometheus | `MetricsRegistry` + `/metrics` Prometheus endpoint（`crates/legion-host/src/metrics.rs`） | Legion 基础设施指标有，业务指标无 |
| **用量/计费** | `/usage`、credit bar、free-usage exhaustion（`xai-grok-pager/src/slash/commands/usage.rs`） | 无 | 缺失 |
| **Prompt dump** | 无 | `--dump-prompts` 写 `~/.legion/dump-prompts/<session>.jsonl`（0600） | Legion 有，Grok 无 |

### 3.2 事件类型对比

Grok session metrics 事件：
- `session_started`
- `turn`
- `turn_completed`
- `doom_loop_recovery`（attempts, accepted_after_budget, top_trigger, model）
- `trace_upload_attempted/succeeded/skipped/failed`
- `upload_reason` 打在 `agent.prompt` span 上

Legion `RunEvent`：
- `Lifecycle { Start, End, Error }`
- `AssistantDelta`
- `ToolStart`
- `ToolEnd`
- `Compaction { summary, boundary, resume_head }`
- `TodoUpdate`

差距：Legion 的 `RunEvent` 是面向客户端的流式事件，不是面向遥测的埋点事件。缺少 turn-level 元数据（token 使用、模型、延迟、恢复行为）。

---

## 4. 工具系统

### 4.1 工具抽象对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **工具 trait** | `Tool` trait + `ToolDispatch` + `ToolStream<TypedToolOutput>` + `ToolCallContext`（`xai-tool-runtime/src/lib.rs`） | `Tool` trait + `ToolContext` + `ToolResult`（`crates/legion-runtime/src/tools.rs`） | 两者类似；Grok 的 `ToolStream` 支持进度流 |
| **工具注册表** | `ToolBridge` 持有 `ToolRegistry` + 动态 MCP 注册；工具集 preset（`grok-build`、`explore`、`plan`、`codex` 等） | `CoreToolRegistry` HashMap；MCP 工具通过 `McpToolAdapter` 合并 | Legion 缺少 preset 概念 |
| **工具分类** | `ToolKind` + `ToolNamespace` + canonical `x.ai/tool` envelope；`is_read_only()` 在 kind 层定义（`xai-grok-tools/src/tool_taxonomy.rs`） | 每个工具自己实现 `is_read_only()`；MCP 工具用 `mcp__<server>__<tool>` 命名 | 缺少统一工具身份/分类 |
| **进度流** | `ToolStream` 可 emit progress updates 和 terminal items | 无进度流，只有 `ToolResult` | 长耗时工具用户体验差 |
| **输出限制** | `DEFAULT_TOOL_OUTPUT_BYTES = 40_000`；MCP 单独 `max_output_bytes` | MCP 描述截断 2048 字符；工具结果由 provider/token 限制 | Legion 缺少显式工具输出字节限制 |

### 4.2 内置工具矩阵对比

| 工具族 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **文件读取** | `read_file`（支持 line range、pdf、pptx、image metadata） | `read`（基础 line range） | Grok 读取能力更丰富 |
| **文件编辑** | `search_replace` | `edit`、`apply_patch`、`write` | 相当 |
| **目录/搜索** | `list_dir`、`grep` | 无 `list_dir`、`grep` | Legion 缺少基础搜索工具 |
| **终端** | `bash`/`run_terminal_command`；streaming、background、timeout、sandbox network | `exec`；sync、timeout、sandbox mode off/restricted/cube | Grok 有后台任务管理；Legion sandbox 选项多但无后台 |
| **后台任务** | `wait_commands_or_subagents` / `kill_command_or_subagent` / `get_command_or_subagent_output` | 无 | 缺失 |
| **网络** | `web_search`、`web_fetch` | `web_search`、`web_fetch` | 相当 |
| **LSP** | `LspTool`（definition/references/diagnostics/format...） | 无 | 缺失 |
| **记忆** | `memory_search`、`memory_get` | `memory_search`、`memory_get`、`memory_index` | Legion 多一个 index |
| **用户交互** | `ask_user_question` | `ask_user` | 相当 |
| **Todo** | `todo_write` | `todo_write` | 相当 |
| **Plan mode** | `enter_plan_mode`、`exit_plan_mode` | 无 | 缺失 |
| **Goal** | `update_goal` | 无 | 缺失 |
| **子 agent** | `task`/`spawn_subagent` | `spawn_subagent`、`run_coordinator`、`swarm_spawn/send/status` | Legion 多 agent 编排更强 |
| **Scheduler** | `scheduler_create`/`delete`/`list` | 无 | 缺失 |
| **图片/视频** | `image_gen`、`image_edit`、`image_to_video`、`reference_to_video`、`video_gen` | `image_generate` | Legion 缺编辑、视频 |
| **语音** | `/voice` STT | `tts` | 方向相反 |
| **浏览器** | 无 | `browser`（CDP） | Legion 领先 |
| **Agent 间消息** | 无 | `agent_to_agent_send` | Legion 领先 |
| **Session 自查询** | 无 | `session_status`、`sessions_list`、`sessions_history` | Legion 领先 |
| **监控** | `monitor` | 无 | Grok 领先 |

### 4.3 Bash / Exec 细节对比

Grok `BashTool` 特性：
- 参数：`command`, `cwd`, `timeout`, `is_background`
- 流式输出返回给 TUI
- 后台任务写入 `~/.grok/sessions/{id}/tasks/{task_id}.log`
- 子进程网络限制通过 seccomp BPF
- 危险命令识别 + sandbox 感知

Legion `ExecTool` 特性：
- 参数：`command`, `cwd`
- `SandboxMode::Off` / `Restricted` / `Cube`
- `RestrictedConfig` 可配置 allowed/denied commands
- 同步执行，返回 `ExecResult { stdout, stderr, exit_code }`
- 无后台任务、无流式、无危险命令白名单

### 4.4 MCP 工具适配对比

| 能力点 | Grok Build | Legion | 差距 |
|---|---|---|---|
| **命名** | `server__tool` | `mcp__<server>__<tool>` | 不同命名约定 |
| **校验** | 严格 regex 校验工具名；disabled tools 缓存 | 无严格校验 | Legion 较松 |
| **描述截断** | 有 MCP 输出字节限制 + 描述截断 | 描述截断 2048 字符 | 相当 |
| **并发** | rmcp transport 级 | stdio=3, remote=20 | 相当 |
| **Credential/OAuth** | 完整 OAuth flow + `$GROK_HOME/mcp_credentials.json` | 检测到 step-up，但未实现 flow | Grok 领先 |

---

## 5. 设计方案：Agent 运行时

### 5.1 新增 `legion-agent` crate（P1）

目标：把 agent 定义与构造从 runtime 中抽离，形成可复用的 `Agent` / `AgentBuilder`。

```text
crates/legion-agent/
├── src/lib.rs
├── src/agent.rs          # Agent 类型，不可变
├── src/builder.rs        # AgentBuilder
├── src/config.rs         # AgentDefinition, PermissionMode, PromptMode, toolset presets
├── src/prompt_context.rs # PromptContext: system prompt + AGENTS.md + personas + skills
├── src/compaction.rs     # CompactionPolicy
└── src/system_reminder.rs # ReminderPolicy
```

最小可运行改动：
1. 将 `legion-core::config::AgentConfig` 扩展为支持 `permission_mode`、`prompt_mode`、`toolset_preset`。
2. 在 `legion-runtime` 中新增 `AgentDefinition` 类型（或从 `legion-agent` 引入）。
3. `AgentRuntime::run` 接收 `AgentDefinition` 而非仅 `RunRequest.system_prompt`，使不同 agent 可携带不同工具集和权限模式。

### 5.2 权限模式扩展（P0）

将 `legion-runtime::approval::Approval` 三态扩展为 `PermissionMode` 六态：

```rust
pub enum PermissionMode {
    Default,           // 未预批准则 prompt
    AcceptEdits,       // 自动批准文件编辑
    Auto,              // 自动批准非危险工具
    DontAsk,           // 无显式允许则 deny
    BypassPermissions, // 自动批准（deny rules / hooks 仍生效）
    Plan,              // plan 模式专用
}
```

实现位置：`crates/legion-runtime/src/approval.rs`。

与现有 `Approval::Off/Prompt/Required` 的映射：
- `Off` → `PermissionMode::BypassPermissions`
- `Prompt` → `PermissionMode::Default`
- `Required` → `PermissionMode::DontAsk`（或保持 `Required` 作为 unattended 必 deny）

引入 `PermissionDecider` trait 替代当前 `CanUseToolFn`：

```rust
#[async_trait]
pub trait PermissionDecider: Send + Sync {
    async fn decide(&self, tool: &str, input: &Value, ctx: &PermissionCtx) -> PermissionDecision;
}

pub enum PermissionDecision {
    Allow,
    Ask { message: String },
    Deny { reason: String },
}
```

注入链：`PreToolUse hook` → `deny rules` → `ask rules` → `remembered grants` → `built-in auto-approvals` → `PermissionMode`。

### 5.3 双阶段 Compaction + Prefire（P1）

在 `crates/legion-runtime/src/compaction.rs` 中新增 `TwoPassCompactor`：

```rust
pub struct TwoPassCompactor {
    single_pass: Compactor,
    split_fraction: f64,      // default 0.95
    prefire_lead_percent: u64, // default 10
}
```

算法：
1. 当 token 达到 `threshold - prefire_lead_percent` 时，后台启动 pass1（总结 95% 前缀）。
2. 当 token 达到 threshold 时，阻塞执行 pass2（将 pass1 的 NOTE₁ 与 5% tail 合并为 NOTE₂）。
3. 使用 fingerprint 校验前缀是否在 pass1 后发生变化（用户编辑/rewind）；若变化则丢弃缓存的 NOTE₁。
4. pass2 失败回退到单阶段 compaction。

需要新增类型：
- `TwoPassSplit`：前缀/尾部分割
- `PrefirePass1`：后台任务句柄
- `CompactNote`：NOTE₁/NOTE₂ 封装

### 5.4 Goal 编排层（P2）

新增 `legion-runtime/src/goal/` 模块：

```rust
pub struct GoalOrchestrator {
    planner: Box<dyn GoalPlanner>,
    strategist: Box<dyn GoalStrategist>,
    classifier: Box<dyn GoalClassifier>,
    tracker: GoalTracker,
}

pub enum GoalEvent {
    PlanCreated { goal: String, steps: Vec<Step> },
    StepStarted { step_id: String },
    StepCompleted { step_id: String, result: String },
    GoalUpdated { goal: String },
}
```

与现有 `FlowRunner` 的关系：
- `GoalOrchestrator` 是 agent 自我规划的动态层；
- `FlowRunner` 是用户声明的静态 DAG；
- 两者可共存：动态 goal 可生成临时 `CoordinatorPlan` 调用 `run_coordinator_plan`。

### 5.5 Plan Mode（P1）

新增文件：
- `crates/legion-runtime/src/plan_mode.rs`：`PlanModeTracker` 状态机
- `crates/legion-tools/src/plan_mode.rs`：`EnterPlanModeTool` / `ExitPlanModeTool`

状态机：

```rust
pub enum PlanModeState {
    Inactive,
    Pending,      // 用户 toggle 开启，但模型还没看到
    Active,       // 计划模式运行中，write 工具被限制到 plan file
    ExitPending,  // 用户 toggle 关闭，等待当前 turn 结束
}
```

行为：
- Active 状态下，只有对 `plan.md` 的写操作被自动批准，其他 write/exec 工具拒绝或 prompt。
- `plan.md` 路径：`~/.legion/sessions/<session_id>/plan.md`。
- 状态持久化到 `plan_mode.json`，支持 resume。

### 5.6 Scheduler Tools（P1）

复用 `legion-automation` 的 cron + task runner，暴露 agent 可调用工具：

```rust
// crates/legion-tools/src/scheduler.rs
pub struct SchedulerCreateTool;
pub struct SchedulerDeleteTool;
pub struct SchedulerListTool;
```

输入：
- `create`: `{ name, cron, prompt, agent_type, enabled }`
- `delete`: `{ id }`
- `list`: `{}`

实现：将请求写入 `cron.jsonl`，由 `CronScheduler` 在到期时触发一个 agent run。

### 5.7 Turn-end TodoGate（P1）

在 `AgentRuntime::run` 的每次迭代末尾增加 `TodoGate`：

```rust
pub struct TodoGate {
    required_patterns: Vec<String>, // 例如 "must have a passing test"
}

impl TodoGate {
    pub fn check(&self, todos: &TodoList, last_assistant_msg: &str) -> TodoGateResult;
}
```

如果 gate 未通过，向模型追加 system-reminder 要求继续完成 todo，而不是提前结束 turn。

---

## 6. 设计方案：可观测性

### 6.1 新增 `legion-telemetry` crate（P1）

结构：

```text
crates/legion-telemetry/
├── src/lib.rs
├── src/client.rs        # TelemetryClient
├── src/unified_log.rs   # JSONL unified log
├── src/session_metrics.rs # lifecycle events
├── src/sentry.rs        # crash reporting (optional)
└── src/otel_layer.rs    # OTLP tracing layer
```

依赖：
- `tracing`, `tracing-subscriber`
- `reqwest`
- `serde_json`
- 可选：`sentry`, `opentelemetry`, `opentelemetry-otlp`

### 6.2 Unified Log（P1）

实现要点：

```rust
pub struct LogEntry {
    pub ts: String,          // RFC 3339
    pub src: LogSource,      // Shell / Cli / Gateway / Channel
    pub pid: u32,
    pub ver: Option<String>,
    pub lvl: LogLevel,
    pub sid: Option<String>, // session id
    pub msg: String,
    pub ctx: Option<Value>,
}
```

- 路径：`~/.legion/logs/unified.jsonl`
- 5 MB 轮替（trim + reopen）
- Gateway 内各组件直接写；CLI/Channel 组件通过 ACP `legion/log` 通知转发
- 提供 `emit!(level, "msg", { ctx })` 宏

### 6.3 Session Metrics（P1）

在 `AgentRuntime::run` 和 `tool_pipeline` 中埋点：

```rust
pub enum SessionMetric {
    SessionStarted { session_id, agent_id, model_ref },
    Turn { session_id, turn_number, input_tokens, model_ref },
    TurnCompleted { session_id, turn_number, output_tokens, tool_calls, duration_ms },
    ToolCalled { session_id, turn_number, tool, read_only, duration_ms },
    Compaction { session_id, turn_number, tokens_before, tokens_after },
    DoomLoopRecovery { session_id, turn_number, attempts, model },
}
```

实现：
- 新增 `TelemetryClient::log_session_event(&self, event: SessionMetric)`
- 在 `MetricsRegistry` 中也暴露为 Prometheus 指标（如 `legion_turns_total`、`legion_tool_calls_total`）

### 6.4 OTLP Tracing（P2）

配置段：

```toml
[telemetry]
enabled = true
mode = "enabled"        # "enabled" | "session_metrics" | "disabled"
unified_log = true
session_metrics = true
mixpanel_token = "..."
events_url = "..."
otlp_endpoint = "..."
otlp_headers = { key = "value" }
sentry_dsn = "..."
```

实现：
- `TelemetryClient::from_config` 初始化 Mixpanel + events HTTP client + OTLP layer
- `tracing_subscriber` 叠加 `EnvFilter` + `fmt layer` + `otel_layer`

### 6.5 与现有 `MetricsRegistry` 的整合

当前 `MetricsRegistry` 用于 MCP/Gateway 指标。扩展：

```rust
impl MetricsRegistry {
    pub fn record_session_metric(&self, event: &SessionMetric) {
        match event {
            SessionMetric::ToolCalled { tool, read_only, .. } => {
                self.increment_counter_with_labels(
                    "legion_tool_calls_total",
                    "total tool calls",
                    &[("tool", tool.clone()), ("read_only", read_only.to_string())],
                );
            }
            // ...
        }
    }
}
```

---

## 7. 设计方案：工具系统

### 7.1 工具分类与 canonical envelope（P1）

在 `legion-runtime` 或 `legion-tools` 新增：

```rust
pub enum ToolKind {
    Read, Write, Edit, Delete, ListDir, Search,
    Execute, Plan, WebSearch, WebFetch,
    BackgroundTaskAction, WaitTasksAction, KillTaskAction,
    Skill, MemorySearch, MemoryGet,
    Task, AskUser, ImageGen, VideoGen,
    Lsp, Monitor, Other,
}

pub enum ToolNamespace {
    Legion,
    Mcp { server: String },
    Plugin { plugin: String },
}

pub struct CanonicalToolMeta {
    pub version: u32,
    pub name: String,
    pub kind: ToolKind,
    pub namespace: ToolNamespace,
    pub label: Cow<'static, str>,
    pub read_only: bool,
    pub input: Option<Value>,
}
```

每个 `Tool` trait 增加：

```rust
fn kind(&self) -> ToolKind;
fn namespace(&self) -> ToolNamespace;
fn canonical_meta(&self, input: &Value) -> CanonicalToolMeta;
```

`ToolEnd` 事件携带 `canonical_meta`，供 telemetry/TUI 统一展示。

### 7.2 新增基础工具（P1）

#### `list_dir` / `grep`

```rust
// crates/legion-tools/src/list_dir.rs
pub struct ListDirTool { policy: Policy }
// schema: { path: string, recursive?: bool }

// crates/legion-tools/src/grep.rs
pub struct GrepTool { policy: Policy }
// schema: { pattern: string, path?: string, glob?: string }
```

#### LSP Tool

```rust
// crates/legion-tools/src/lsp.rs
pub struct LspTool { policy: Policy }
// schema: { action: "definition" | "references" | "diagnostics" | "format", path: string, line: number, column: number }
```

需要抽象 `LspBackend` trait，支持多种 LSP server：

```rust
#[async_trait]
pub trait LspBackend: Send + Sync {
    async fn definition(&self, path: &Path, line: usize, col: usize) -> Result<String, LspError>;
    async fn references(&self, path: &Path, line: usize, col: usize) -> Result<String, LspError>;
    async fn diagnostics(&self, path: &Path) -> Result<String, LspError>;
    async fn format(&self, path: &Path) -> Result<String, LspError>;
}
```

### 7.3 后台任务管理工具（P1）

新增：

```rust
// crates/legion-tools/src/background_task.rs
pub struct ExecBackgroundTool;     // exec 支持 is_background
pub struct WaitTasksTool;
pub struct KillTaskTool;
pub struct GetTaskOutputTool;
```

实现：
- `ExecTool` 增加 `is_background` 参数；后台任务写入 `~/.legion/sessions/<session_id>/tasks/<task_id>.log`
- 新增 `BackgroundTaskRegistry` 管理任务句柄
- `WaitTasksTool` 阻塞等待指定任务完成并返回输出
- `KillTaskTool` 发送 SIGTERM/SIGKILL

### 7.4 多媒体工具扩展（P2）

#### `image_edit`

复用 `ProviderRouter::generate_image` 的 edit 变体：

```rust
pub struct ImageEditTool { policy: Policy, router: Arc<ProviderRouter> }
// schema: { image_path: string, prompt: string, n?: number, size?: string }
```

#### `video_generate`

```rust
pub struct VideoGenerateTool { policy: Policy, router: Arc<ProviderRouter> }
// schema: { prompt: string, image_path?: string, duration?: number }
```

### 7.5 Bash/Exec 增强（P1）

在 `ExecTool` 中增加参数：

```rust
pub struct ExecInput {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout: Option<u64>,
    pub is_background: Option<bool>,
}
```

实现流式输出：

```rust
pub enum ToolStreamItem {
    Progress(String),
    Result(ToolResult),
}
```

`Tool` trait 增加可选方法：

```rust
async fn execute_streaming(&self, input: Value, ctx: ToolContext) -> Result<BoxStream<ToolStreamItem>, ToolError>;
```

默认实现退化为 `execute` 后一次性返回 `Result`。

### 7.6 MCP 工具校验与 OAuth（P1）

在 `legion-mcp` 中：

```rust
pub struct McpCredentialsStore {
    path: PathBuf, // ~/.legion/mcp_credentials.json
}

impl McpCredentialsStore {
    pub async fn get(&self, server: &str) -> Option<McpCredentials>;
    pub async fn set(&self, server: &str, creds: McpCredentials);
}
```

增加 OAuth flow：

```rust
// crates/legion-mcp/src/oauth.rs
pub async fn perform_oauth(flow: OAuthFlowConfig) -> Result<McpCredentials, McpOAuthError>;
```

工具名校验：

```rust
lazy_static! {
    static ref MCP_TOOL_NAME_RE: Regex = Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").unwrap();
}
```

---

## 8. 落地优先级与里程碑

### Phase 1：安全与基础（P0，2-3 周）

1. 扩展 `PermissionMode` 六态，替换/兼容现有 `Approval` 三态。
2. 引入 `PermissionDecider` trait，支持 hooks + rules 注入点。
3. 在 `legion-runtime` 中补齐 `list_dir`/`grep` 等基础工具。

### Phase 2：Agent 体验（P1，3-4 周）

1. 新建 `legion-agent` crate，抽象 `AgentBuilder` + `AgentDefinition`。
2. 实现 Plan mode 状态机与 tools。
3. 实现 Scheduler tools，复用 `legion-automation` cron。
4. 实现后台任务管理工具（`wait_tasks`/`kill_task`/`get_task_output`）。

### Phase 3：Compaction 与 Goal（P1，2-3 周）

1. 双阶段 compaction + prefire。
2. `TodoGate` turn-end 完成条件校验。
3. 轻量 Goal tracker（可选，先不引入完整 planner）。

### Phase 4：可观测性（P1，2 周）

1. 新建 `legion-telemetry` crate。
2. Unified log + session metrics。
3. Prometheus 业务指标接入 `/metrics`。

### Phase 5：工具生态扩展（P2，按需）

1. Tool taxonomy + canonical envelope。
2. LSP tool。
3. image_edit / video_generate。
4. OTLP tracing / Sentry（可选）。

---

## 9. 需要避免的反模式

1. **不要把 Grok 的 TUI 深度照搬到 Legion**：Legion 的核心价值是多通道 gateway，TUI 增强是 P2。
2. **不要破坏 `legion-cli` 不依赖 `legion-gateway` 的规则**：任何新增 crate 都需验证 `cargo tree -p legion-cli` 不含 `legion-gateway`。
3. **不要一次性重写 agent loop**：先通过扩展 `AgentRuntime` 的 `RunRequest` 和新增 trait 引入能力，避免大规模重构。
4. **不要为 telemetry 引入过多外部依赖**：Mixpanel/Sentry/OTLP 都做成可选 feature，默认关闭。
5. **不要改变 MCP 命名空间**：`mcp__<server>__<tool>` 是 Legion 的 wire contract，tool taxonomy 是附加元数据，不要替换它。

---

## 10. 关键验证命令

```bash
# 1. 确保 CLI 仍不依赖 Gateway
cargo tree -p legion-cli | rg 'legion-gateway' && echo "FAIL" || echo "OK"

# 2. 新增 crate 编译
cargo check -p legion-agent -p legion-telemetry

# 3. Runtime 测试
cargo test -p legion-runtime

# 4. Tools 测试
cargo test -p legion-tools

# 5. 全量门禁
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
cargo fmt -- --check
```

---

*文档位置：`docs/design/grok-cli-agent-obs-tools-design.md`*
