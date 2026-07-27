# Grok CLI 与 Legion 功能差距分析

> 本文档将 SpaceXAI 的 `grok` CLI/TUI（仓库 `grok-build`）与 Legion 进行功能节点级对比，识别 Legion 当前缺失或可深化的能力节点，并给出可借鉴的设计要点。注意：两者定位不同——Grok 是面向单用户的终端 AI 编码助手；Legion 是多通道 AI agent gateway。因此差距不等于缺陷，需结合 Legion 路线图判断取舍。

---

## 1. 分析范围与方法

- **Grok Build 版本**：从 `https://github.com/xai-org/grok-build` 克隆的当前 `main` 树。
- **Legion 版本**：当前工作区 `/Users/ringconn/workspace/projects/legion` 的源码。
- **粒度**：先拆功能域（运行形态、TUI、Agent 运行时、工具、Workspace、MCP、Memory、安全、配置、Session、可观测性、分发），再拆到具体功能节点与设计细节。
- **信息来源**：
  - Grok：`crates/codegen/xai-grok-pager/docs/user-guide/`、`README.md`、核心 crate 源码（`xai-grok-pager`、`xai-grok-shell`、`xai-grok-agent`、`xai-grok-tools`、`xai-grok-workspace`、`xai-grok-mcp`、`xai-grok-memory`、`xai-grok-sandbox`、`xai-grok-config`、`xai-grok-telemetry`、`xai-grok-update`）。
  - Legion：`docs/design/`、`docs/design/gaps/00-overview.md`、`AGENTS.md`、核心 crate 源码（`legion-cli`、`legion-host`、`legion-runtime`、`legion-gateway`、`legion-tools`、`legion-mcp`、`legion-memory`、`legion-channel`、`legion-provider`、`legion-protocol`、`legion-automation`、`legion-skills`、`legion-acp`）。

---

## 2. 高层结论

| 维度 | Grok CLI | Legion | 核心差距 |
|---|---|---|---|
| **产品定位** | 单用户终端 AI 编码 agent，强调沉浸式 TUI | 多通道 AI agent gateway，强调通道/自动化/多 agent 编排 | 定位不同，但 Legion 的 CLI 面明显更薄 |
| **交互深度** | 全屏 TUI、40+ 斜杠命令、Dashboard、主题、语音 | TUI 基础（`legion-cli/src/tui.rs`）、斜杠命令有限 | Legion CLI 远未达到 Grok 的终端体验深度 |
| **Agent 内核** | AgentBuilder、compaction 预触发、goal 编排、plan mode、scheduler、turn-end gating | Agent loop、compaction、subagent/coordinator/swarm、prompt management、goal turns + goal 工具、scheduler 工具 | Legion 内核基本具备；plan mode 与完整 goal 编排（planner/strategist 等）仍缺 |
| **工具丰富度** | 文件/终端/搜索/web/LSP/子 agent/调度/图片视频生成/监控 | 文件/终端/web/子 agent/coordinator/图片/TTS/浏览器/session 工具 | 互有覆盖；Grok 在 LSP、scheduler、视频生成上领先；Legion 在多 agent 编排、浏览器、通道原生工具上领先 |
| **Workspace/VCS** | 深度集成：worktree、checkpoint、rewind、hunk tracker、folder trust、`.envrc` | 仅有 `exec` sandbox 和基础工作目录 | Legion 几乎无 VCS 工程化能力 |
| **MCP** | rmcp、OAuth、streamable HTTP、liveness、credentials store | 四种传输、metrics、session 过期重连、adapter | Grok 在 OAuth/credential 管理上更完整 |
| **Memory** | Markdown 文件 + sqlite-vec + FTS5 + embedding + Dream 合并 | SQLite+sqlite-vec + FTS5 + 衰减/合并 | Grok 的“文件化记忆 + Dream”更偏用户可感知 |
| **安全** | Landlock/Seatbelt/bwrap 内核沙箱 + 权限模式 + hooks + 危险命令列表 | exec sandbox + Gateway loopback + channel access | Legion 缺少 OS 级内核沙箱 |
| **配置** | 分层 managed config + requirements.toml + MDM | 单一 `config.toml` + env 覆盖 | Legion 配置体系较简单 |
| **Session** | fork/rewind/restore/import/background tasks/leader 重连回放 | resume/orphan repair/TTL/archive | Legion 恢复机制有，但用户交互面弱 |
| **可观测性** | unified log + Mixpanel + Sentry + OTLP + dashboard | tracing + logging + 少量 metrics | Legion 缺少产品级遥测闭环 |
| **分发** | 自动更新、alpha/stable/enterprise 通道、install 脚本、npm、leader 自更新 | CLI/Gateway 独立分发、签名 manifest、upgrade/rollback | 两者方向不同，Legion 的 Gateway 独立分发是优势 |

---

## 3. 逐域功能节点对比

### 3.1 运行形态与入口

#### Grok CLI 功能节点

| 节点 | 行为 | 关键配置/CLI | 源码位置 |
|---|---|---|---|
| 交互式 TUI | 全屏终端， Alt-screen 默认 | `grok [PROMPT]`、`--no-alt-screen` 内嵌 | `xai-grok-pager/src/app/mod.rs` |
| 单轮无头 | 输出一次回复后退出，可等后台任务 | `-p/--single`、`--output-format plain|json`、`--json-schema`、`--check` | `xai-grok-pager/src/app/cli.rs` |
| 持续无头 | 持续会话，WebSocket relay | `grok agent headless --grok-ws-url` | `xai-grok-shell/src/agent/app.rs` |
| stdio ACP | JSON-RPC over stdin/stdout，供 IDE 嵌入 | `grok agent stdio [--model][--yolo][--reauth]` | `xai-grok-shell/src/agent/app.rs` |
| WebSocket 服务 | 外部客户端通过 WebSocket 连接 | `grok agent serve --bind --secret` | `xai-grok-shell/src/agent/app.rs` |
| Leader 模式 | 单机单实例 IPC 服务端，客户端通过 Unix socket 连接 | `grok agent leader --no-exit-on-disconnect` | `xai-grok-shell/src/leader/` |
| Dashboard | 原生 Dashboard 视图 | `grok dashboard` | `xai-grok-pager/src/views/dashboard.rs` |
| Wrap | 用本地 PTY 包装命令并转发 OSC 52 | `grok wrap <cmd>` | `xai-grok-pager/src/wrap_cmd.rs` |
| Worktree 恢复 | 在新 git worktree 中恢复会话 | `--resume [SESSION_ID]`、`--worktree [NAME]`、`--restore-code` | `xai-grok-shell/src/session/worktree.rs` |

#### Legion 功能节点

| 节点 | 行为 | 关键配置/CLI | 源码位置 |
|---|---|---|---|
| 嵌入式 Agent | `legion agent` 通过 `legion-host::AgentHost` 在进程内运行 | `--yolo`、`--dump-prompts`、`--wait`、`--session` | `crates/legion-cli/src/main.rs`、`driver.rs` |
| Gateway 模式 | CLI 管理独立 Gateway 二进制 | `legion gateway start|stop|install|upgrade|rollback|status|doctor` | `crates/legion-cli/src/gateway_manager.rs` |
| ACP 外部 harness | 可 spawn 外部 ACP agent | model ref 以 `acp:` 前缀匹配 | `crates/legion-acp/src/harness.rs` |

#### Gap

- **多模式入口缺失**：Legion CLI 只有“嵌入式 Agent”和“Gateway 管理”两种形态，没有 `stdio`、`serve`、`leader`、`dashboard`、`wrap` 等模式。
- **ACP 仅骨架**：`legion-acp` 的 plugin 是 stub（`crates/legion-acp/src/plugin.rs:22-38`），未真正接入 runtime 作为 stdio server。
- **无头输出单一**：Legion 无 `output-format plain|json|streaming-json`、`--json-schema`、`--best-of-n`、`--check` 等无头/CI 友好选项。
- **无 leader/relay 架构**：Legion 没有进程间共享 agent 实例的 leader-follower 模型，每次启动都重建完整 runtime。

---

### 3.2 TUI 与交互

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| 屏幕模式 | fullscreen（默认）、inline（`--no-alt-screen`）、minimal（`--minimal`，scrollback-native） |
| 核心视图 | chat scrollback、prompt widget、block viewer、completion/slash 下拉、tasks pane、todo pane、subagent catalog、queue pane、plan approval、history search、file search、welcome、status bar、context/credit/shortcuts bar |
| 模态弹窗 | settings、memory、MCPs、extensions、agents、personas、dashboard、import Claude、new worktree、permissions、rewind、session picker、session title、question view |
| 斜杠命令 | 40+ 条：session lifecycle、FS、model/effort、context/memory、plan、tools/tasks、plugin/marketplace、share/export、settings/UI、help/info、media generation、voice、utility |
| 交互机制 | 同步 reducer：`Action -> (state, Vec<Effect>)`；slash 使用 fuzzy matcher + MRU |
| 主题 | `GrokDay`、`GrokNight`、`TokyoNight`；OSC 11/4 颜色探测；cursor styling |
| 语音 | `/voice` 流式 STT；macOS/Windows 用 `cpal`，Linux shell 到 `pw-record`/`parec`/`arecord` |
| 媒体生成 | `/imagine`、`/imagine-video` |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| TUI | 基础 TUI（`legion-cli/src/tui.rs`），具体视图有限 |
| 斜杠命令 | `slash_commands.rs` 中实现，数量远少于 Grok |
| 输出 | 主要依赖滚动日志/文本输出 |

#### Gap

- **TUI 深度差距仍大**：Legion 已有 fullscreen/inline 两种屏幕模式（`/mode` 切换，经 `[tui]` 配置持久化），但没有 minimal（scrollback-native）模式、没有 block viewer、没有 file search/project picker、没有 dashboard。
- **斜杠命令生态薄弱**：Grok 的 40+ 命令覆盖 session、model、context、plan、tools、plugins、settings、help、media；Legion 缺少 `/compact`、`/fork`、`/rewind`、`/context`、`/plan`、`/tasks`、`/queue`、`/skills`、`/mcps`、`/voice`、`/imagine` 等。
- **无语音输入**：Legion 有 TTS（`TtsTool`）但无 STT/语音交互。
- **无媒体生成 UI**：Legion 的 `image_generate` 是 tool，没有 `/imagine` 这样的 TUI 快捷入口；无视频生成。
- **主题/可访问性**：Legion TUI 已有主题系统（`/theme` + `[tui]` 配置持久化），但主题仅限 dark/light（无用户自定义主题），缺少 OSC 颜色探测（终端主题自动检测）与 cursor styling。
- **Gateway 模式取消不受支持**：Esc 取消仅 local 模式可用；`agent.cancel` RPC 尚未实现，gateway 模式下 WsDriver 返回明确错误提示（需跨 crate 协议变更）。
- **Setup 向导未共享 TUI 主题**：`setup.rs` 是独立的行式 UI（非 ratatui），不随 TUI 主题变化，统一主题化是独立工程。

---

### 3.3 Agent 运行时

#### Grok CLI 功能节点

| 节点 | 细节 | 源码 |
|---|---|---|
| Agent 构造 | `AgentBuilder` 从 `AgentDefinition` + `PromptContext` + `ToolBridge` 构建不可变 `Agent` | `xai-grok-agent/src/{agent.rs,builder.rs}` |
| Agent 模式 | `Tui`、`Headless`、`Stdio`、`Serve`、`Leader`、`Generic` | `xai-grok-shell/src/agent/config.rs` |
| 权限模式 | `default`、`acceptEdits`、`auto`、`dontAsk`、`bypassPermissions`、`plan` | `xai-grok-agent/src/config.rs` |
| Prompt 模式 | `extend`（默认）、`full` | `xai-grok-agent/src/config.rs` |
| Compaction | 双阶段 compaction：prefire pass-1 + pass-2 apply；阈值 `GROK_PREFIRE_LEAD_PERCENT`；manual `/compact`；checkpoint persistence；segments mode | `xai-grok-shell/src/session/compaction.rs`、`two_pass.rs` |
| Goal 编排 | planner、strategist、summarizer、stop detector、classifier、next-step、tracker、orchestrator | `xai-grok-shell/src/session/goal_*.rs` |
| Plan mode | `enter_plan_mode`/`exit_plan_mode` tools；plan approval view；plan resume after restart | `xai-grok-shell/src/session/plan_mode.rs` |
| 子 agent | `TaskTool`  spawn subagents；inherited/shared toolset；MCP pool 共享 | `xai-grok-tools/src/implementations/grok_build/task/` |
| Scheduler | `scheduler_create`/`delete`/`list`；`/loop` 创建定时任务 | `xai-grok-tools/src/implementations/grok_build/scheduler/` |
| Turn-end gating | `TodoGate` 与完成条件检查 | `xai-grok-shell/src/session/turn_completion.rs` |

#### Legion 功能节点

| 节点 | 细节 | 源码 |
|---|---|---|
| Harness | `Harness::run(RunRequest) -> RunStream` | `legion-runtime/src/lib.rs` |
| Agent loop | provider chat + tool calls | `legion-runtime/src/agent_loop.rs` |
| Approval gate | `ApprovalGate` 交互式审批 | `legion-runtime/src/approval.rs` |
| Compaction | 基础 compaction | `legion-runtime/src/compaction.rs` |
| Subagent | `spawn_subagent`（Typed/Fork）、`run_coordinator`、`SwarmManager` | `legion-runtime/src/{subagent.rs,coordinator.rs,swarm.rs}` |
| Prompt management | `SystemPromptBuilder` section 化、override 优先级链、`--dump-prompts`、`legion context` | `legion-host/src/` |
| Task Flow DAG | 声明式 `flows` + `FlowRunner` | `legion-automation/src/flow.rs` |
| Standing Orders / Inferred Commitments | cacheable prompt section、轻量 LLM 抽取 commitment | `legion-automation/src/` |
| Goal mode | `GoalGate` turn-end 自动续轮（goal turns，无 turn 上限）、`get_goal`/`create_goal`/`update_goal` 工具、`/goal` 立即开跑、`[goals]` 配置 | `legion-runtime/src/{goal.rs,goal_gate.rs}`、`legion-host/src/goal_tools.rs` |

#### Gap

- **Goal 编排层仍缺**：goal turns + model-facing 工具已落地（2026-07-17，`GoalGate` + goal tools）；仍缺 planner/strategist/classifier/orchestrator 这一 agent 自我规划层，任务流仍是声明式 DAG。
- **Plan mode 缺失**：Legion 有 Task Flow，但缺少面向用户的 plan mode（`enter_plan_mode`/`exit_plan_mode` tools + plan approval view）。
- **Scheduler 缺失**：Legion 的 cron 调度在 `legion-automation` 中，但缺少 agent 可调用 的 `scheduler_create/delete/list` tools 以及 `/loop` 斜杠命令。
- **Compaction 深度不足**：Grok 有双阶段 compaction + prefire；Legion compaction 相对简单（参见 `docs/design/gaps/03-shallow/compaction.md`）。
- **权限模式单一**：Legion 只有 `Approval::Off/Prompt/Required`，没有 Grok 的 `default/acceptEdits/auto/dontAsk/bypassPermissions/plan` 这套完整模式。

---

### 3.4 工具系统

#### Grok CLI 内置工具

| 工具 | 说明 |
|---|---|
| `read_file` | 文件读取 |
| `search_replace` | 编辑 |
| `grep` | 内容搜索 |
| `list_dir` | 目录列表 |
| `bash` / `run_terminal_cmd` | 终端命令；支持 streaming、background、timeout、网络限制 |
| `wait_tasks` / `kill_task` / `get_terminal_command_output` | 后台任务管理 |
| `web_search` / `web_fetch` | 网络搜索/抓取 |
| `memory_search` / `memory_get` | 记忆检索 |
| `lsp` | LSP 客户端工具 |
| `ask_user_question` | 向用户提问 |
| `enter_plan_mode` / `exit_plan_mode` | 计划模式 |
| `todo_write` | Todo 管理 |
| `update_goal` | Goal 更新 |
| `task` | 子 agent |
| `scheduler_create/delete/list` | 定时任务 |
| `image_gen` / `image_edit` | 图片生成/编辑 |
| `image_to_video` / `reference_to_video` / video gen | 视频生成 |
| `monitor` | 监控 |
| `deploy_app` | 部署 stub |

#### Legion 内置工具

| 工具 | 说明 |
|---|---|
| read / write / edit / apply_patch | 文件操作 |
| exec | 执行命令；sandbox |
| web_search / web_fetch | 网络 |
| memory_* | 记忆 |
| spawn_subagent | 子 agent |
| run_coordinator | 协调器 |
| agent_to_agent_send | agent 间消息 |
| swarm_* | Swarm |
| image_generate | 图片生成 |
| tts | 语音合成 |
| browser | 浏览器（CDP） |
| session_status / sessions_list / sessions_history | 会话自查询 |

#### Gap

- **LSP 工具缺失**：Legion 没有 LSP 客户端工具，无法让 agent 做代码补全/定义跳转/诊断。
- **Scheduler 工具缺失**：见 3.3。
- **视频生成缺失**：Legion 有 `image_generate` 和 `tts`，无视频生成。
- **图片编辑缺失**：Legion 有图片生成，无 `image_edit`。
- **监控工具缺失**：Grok 有 `monitor`，Legion 无。
- **终端工具深度**：Grok 的 bash 支持 background task、timeout、网络限制；Legion 的 `exec` 有 sandbox 但缺少后台任务管理工具（`wait`/`kill`/`get_output`）。
- **跨 harness 工具身份**：Grok 有 `tool_taxonomy.rs` 和 `x.ai/tool` 标准 envelope；Legion 工具命名空间是 `mcp__<server>__<tool>`，缺少统一的工具分类/只读标识。

---

### 3.5 Workspace 与 VCS

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| 双模式 workspace | Local（直接 `WorkspaceHandle`）/ Proxy（hub WebSocket） |
| 文件系统抽象 | Local FS、ACP-backed FS、mock FS、file tree、codebase index、fuzzy search |
| Git / worktrees | 创建/列出/删除 worktree、git root 检测、checkout persisted HEAD |
| jj 支持 | jj workspace 检测 |
| Foreign sessions | 导入/恢复 Claude、Codex 会话 |
| Folder trust | 信任存储、信任冲突、权限管理 |
| Hunk tracker | 跨会话编辑 hunk 跟踪 |
| Project config / `.envrc` | `.grok/` 项目配置、`.envrc` 捕获 |
| Checkpoints / rewind | `RewindPoint`、merge/truncate、代码恢复 |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| 工作目录 | `AgentParams.workspace` 可覆盖（仅 embedded 模式有效） |
| exec sandbox | 受限 profile；必须显式失败而非静默回退 |
| Gateway loopback | 默认绑定 loopback，拒绝 `auth.mode: none` |

#### Gap

- **几乎是空白**：Legion 没有 workspace server、VCS 集成、worktree、checkpoint、rewind、hunk tracker、folder trust、project config。
- **工程体验差距**：Grok 把“理解代码库”作为核心能力；Legion 当前把代码库当作一个工作目录 + exec sandbox。
- **借鉴点**：可引入 `WorkspaceHandle` + git status/worktree 抽象，至少让 CLI 模式具备代码库感知和编辑撤销能力。

---

### 3.6 MCP

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| 传输 | stdio child process、`StreamableHttpClientTransport`、ACP reverse-channel SDK servers |
| OAuth | RFC 8414 discovery、DCR、PKCE；浏览器 OAuth；BYO OAuth config |
| 凭证 | `$GROK_HOME/mcp_credentials.json`；两层去重（in-process watch + filesystem lock） |
| 生命周期 | `InitProgress` 状态机；liveness poller；失败重启 |
| 工具命名 | `server__tool`；严格跨 provider regex 校验；disabled tools 缓存 |
| 隔离 | `xai-grok-mcp` 隔离 `rmcp` 2.1 和 `reqwest` 0.13，避免与 workspace 其余部分冲突 |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| 传输 | `stdio`、`http`、`sse`、`ws` 四种 `McpTransport` |
| 客户端 | `StdioMcpClient`、`HttpMcpClient`、`WsMcpClient`、`SseMcpClient`；session 过期重连（`-32001`）；OAuth step-up 检测 |
| Manager | 并发限流（stdio=3，remote=20）；auth cache 15 分钟；`auto_approve` 列表 |
| Adapter | `mcp__<server>__<tool>`；描述截断 2048 字符；resilient call |
| Metrics | Prometheus `mcp_calls_total` / `mcp_errors_total` |

#### Gap

- **OAuth 与凭证管理**：Legion 检测到 OAuth step-up，但缺少 Grok 那样的完整 OAuth flow（discovery/DCR/PKCE）和持久化凭证存储。
- **Streamable HTTP**：Legion 有 HTTP/SSE/WS，但无 Grok 的 `StreamableHttpClientTransport`（MCP 2024-11 spec 的 streamable HTTP）。
- **Liveness 与自动重启**：Legion 有 reconnect，但缺少 Grok 的 `InitProgress` 状态机 + liveness poller 自动重启。
- **工具校验**：Grok 对 MCP 工具有严格 regex 校验和 disabled tools 缓存；Legion 仅有描述截断。

---

### 3.7 Memory

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| 存储布局 | `~/.grok/memory/MEMORY.md`（全局）、`~/.grok/memory/{workspace_hash}/MEMORY.md`（工作区）、`sessions/YYYY-MM-DD-{slug}-{sid8}.md`（会话日志） |
| 索引 | Markdown chunking、hash-based 增量重索引、SQLite `chunks` + FTS5 + `sqlite-vec` |
| 检索 | Hybrid FTS5 BM25 + vector KNN；score 归一化；时间衰减；source 权重；MMR diversity；min-score filtering |
| Embeddings | OpenAI-compatible embeddings；batch=32；sqlite-vec 缓存 |
| Dream 合并 | AutoDream 按 `enabled`/`min_hours`/`min_sessions` 合并会话日志为长期 `MEMORY.md` |
| Watcher | 监听 memory 文件变化并重索引 |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| 后端 | `SqliteVecBackend`：documents + FTS5 + document_vec |
| 检索 | vector + FTS5  reciprocal rank fusion；decay；deterministic merge |
| Embedder | `FakeEmbedder`（测试）、`ProviderEmbedder`（委托 provider router） |
| 分层 | 已实现 Phase A/B/C：权重、去重、auto_extract、secret scanning、衰减、merge、LLM 召回、`recall.limit`、`SurfacedStore` 去重 |

#### Gap

- **用户可感知记忆文件**：Grok 把记忆以 Markdown 文件形式暴露给用户；Legion 的记忆完全在 SQLite 中，用户无法直接查看/编辑。
- **Dream 合并**：Legion 缺少跨会话自动合成长期记忆的机制。
- **Watcher**：Legion 没有文件监听重索引。
- **Embedding 缓存**：Legion 的 `ProviderEmbedder` 直接调用 provider，缺少 sqlite-vec 缓存层。
- **时间衰减与 MMR**：Legion 有 decay 和 RRF，但 MMR diversity re-ranking 未明确落地。

---

### 3.8 认证与安全

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| 认证 | xAI OAuth2、device-code flow、`grok login`/`logout`、 proactive token refresh、system-power suspend gate |
| API keys | `EnvKeys` 支持一个或多个 env var 名 |
| Secrets | `$GROK_HOME/auth.json`、secure credential storage |
| Trust | `--trust`、folder trust store、trust conflicts |
| 权限规则 | CLI `--allow`/`--deny`、配置 allow/deny rules、remembered grants、hooks |
| 权限模式 | `default`、`acceptEdits`、`auto`、`dontAsk`、`bypassPermissions`、`plan` |
| Sandbox | Landlock（Linux）/ Seatbelt（macOS）/ bwrap re-exec；profiles：`workspace/devbox/read-only/strict/off` + custom；seccomp BPF 子进程网络阻断 |
| 危险命令 | 内置只读命令列表（`ls`、`cat`、`git status` 等）；危险命令列表 |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| Gateway auth | token 或 password；`none` 在非 loopback 拒绝 |
| Auth profiles | token/password/AWS SigV4 |
| API keys | 属于 auth profiles 或 env var，不提交到配置 |
| Tool approval | `Approval::Off` / `Prompt` / `Required`；unattended 拒绝 `Prompt`/`Required` |
| Channel access | `AccessPolicy`、`BotLoopGuard` |
| Exec sandbox | restricted profiles 必须显式失败；`cube` 可用性回退例外 |
| 分发安全 | manifest Ed25519 签名、SHA-256、HTTPS-only、install lock |

#### Gap

- **OS 级沙箱缺失**：Legion 的 sandbox 是 exec profile 级别，没有 Landlock/Seatbelt/bwrap 内核级隔离。
- **权限系统单薄**：Legion 只有 `Off/Prompt/Required`，缺少 allow/deny rules、remembered grants、hooks、权限模式。
- **OAuth 用户认证缺失**：Legion 没有面向终端用户的 OAuth/device-code login flow；认证是面向 Gateway/channel 的。
- **危险命令列表**：Legion 没有 Grok 那样细粒度的只读命令白名单和危险命令识别。

---

### 3.9 配置

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| 分层加载 | `/etc/grok/managed_config.toml` → `$GROK_HOME/managed_config.toml` → `$GROK_HOME/config.toml` → `$GROK_HOME/requirements.toml` → `/etc/grok/requirements.toml` → macOS MDM managed preferences |
| Setup 命令 | `grok setup [--json]` 拉取并安装托管配置 |
| 关键段 | `[endpoints]`、`[features]`、`[cli]`、`[ui]`、`[mcp_servers]`、`[memory]`、`[sandbox]`、`[telemetry]` |
| 版本覆盖 | 每层可应用 version override；requirements 可 fail-closed |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| 配置模式 | `crates/legion-core/src/config.rs` 统一定义 |
| 覆盖 | env var 解析、defaults |
| 关键段 | flows、channels、mcp_servers、memory、decay、merge、orphan policy、task、automation |

#### Gap

- **托管配置与企业管控**：Legion 缺少 `/etc/` 级 managed config、requirements.toml、MDM 集成。
- **Setup 命令**：Legion 没有 `legion setup` 来拉取组织级配置。
- **Telemetry 配置**：Legion 没有 Mixpanel/Sentry/OTLP 等产品遥测配置段。

---

### 3.10 Session 管理

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| 持久化 | `~/.grok/sessions/`；JSONL chat history；`summary.json`；`signals.json`；background task manifest |
| 存储模式 | `local`（默认）、`writeback`（HTTP flush） |
| Resume / continue / fork | `--resume [SESSION_ID]`、`--continue`、`--session-id`、`--fork-session`；跨 worktree 解析 |
| Rewind | `RewindPoint`、merge/truncate |
| Restore / import | 远程 session state + memory archive 下载到 worktree；恢复代码 checkout |
| Background tasks | per-session registry、输出日志、manifest for resume |
| Sharing / export / trace | `grok share`、`grok export`、`grok trace` |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| Resume | `RunEvent::Compaction` 携带 `resume_head`、`load_for_resume` |
| Orphan repair | `transcript_repair.rs` + `sessions.orphanPolicy` |
| TTL / archive | `sessions.ttlDays`/`archiveDir`；gateway 启动执行归档 |
| Lite read | `lite_read`/`list_session_summaries` 头读摘要 |
| Session 工具 | `session_status`、`sessions_list`、`sessions_history`（仅当前 agent） |

#### Gap

- **用户级 session UI**：Legion 的 session 管理偏 runtime 内部；没有 session picker、fork、rewind、share/export/trace 的用户界面。
- **Background tasks**：Legion 的 cron/task runner 是系统级，不是 per-session background task。
- **Foreign session import**：Legion 无法导入 Claude/Codex 会话。
- **Worktree 级恢复**：Legion 没有 worktree + 代码 checkout 恢复能力。

---

### 3.11 可观测性与分发

#### Grok CLI 功能节点

| 节点 | 细节 |
|---|---|
| Telemetry | Mixpanel product events、session-scoped context；`session_metrics` 模式 |
| Sentry | crash/error reporting |
| Unified log | `~/.grok/logs/unified.jsonl`；shell 直接写，pager/desktop 通过 ACP `x.ai/log`；5 MB 轮替 |
| OTLP | 内部 trace pipeline + 外部 OTEL stream（`GROK_EXTERNAL_OTEL`） |
| Session metrics | `session_started`、`turn`、`turn_completed`、`doom_loop_recovery`、trace upload |
| Usage / dashboard | `/usage`、credit bar、免费额度耗尽检测 |
| 更新 | channels `stable/alpha/enterprise`；install.sh、npm `@xai-official/grok`、GitHub Releases；leader 自更新 |

#### Legion 功能节点

| 节点 | 细节 |
|---|---|
| Tracing | `tracing` + logging |
| Metrics | Prometheus 指标（MCP、Gateway） |
| 分发 | CLI/Gateway 独立二进制；`ReleaseManifest`；Ed25519 签名；upgrade/rollback/prune/doctor；协议兼容性检查 |

#### Gap

- **产品遥测闭环缺失**：Legion 缺少 unified log、Mixpanel、Sentry、session metrics 等产品级遥测。
- **Dashboard**：Legion 没有 usage dashboard 或 native dashboard view。
- **CLI 自更新**：Legion 的 Gateway 有 upgrade/rollback，但 CLI 自身没有 Grok 式的自动更新通道。
- **OTLP**：Legion 没有外部 OTEL exporter 配置。

---

## 4. 可直接借鉴的设计点（高价值、低耦合）

| 设计点 | 来源 | 价值 | 落地建议 |
|---|---|---|---|
| **同步 Action -> (state, Effects) reducer** | `xai-grok-pager/src/app/dispatch/mod.rs` | 让 TUI 逻辑可测试、无 async | Legion TUI 重构时采用 |
| **Tool taxonomy + canonical `x.ai/tool` envelope** | `xai-grok-tools/src/tool_taxonomy.rs` | 统一工具身份、只读分类、跨 harness 对齐 | 在 `legion-tools` 引入 `ToolKind`/`ToolNamespace` |
| **双阶段 compaction + prefire** | `xai-grok-shell/src/session/two_pass.rs` | 减少 context window 峰值 | 升级现有 compaction（见 `03-shallow/compaction.md`） |
| **权限模式 + rules + hooks + remembered grants** | `xai-grok-workspace/src/permission/` | 细粒度安全控制 | 替代当前 `Approval::Off/Prompt/Required` |
| **OS 级 sandbox（Landlock/Seatbelt/bwrap）** | `xai-grok-sandbox/src/` | 真正隔离 exec | 优先级 P0（已有 `03-shallow/sandbox-isolation.md`） |
| **Workspace handle + worktree + checkpoint** | `xai-grok-workspace/src/` | 代码库工程化 | 新增 workspace crate 或扩展 `legion-host` |
| **MCP credential store + OAuth flow** | `xai-grok-mcp/src/oauth.rs`、`credentials.rs` | 企业级 MCP 接入 | 扩展 `legion-mcp` |
| **Markdown 记忆文件 + Dream 合并** | `xai-grok-memory/src/` | 用户可感知、可审计的记忆 | 在 `legion-memory` 之上加文件化层 |
| **Unified log + session metrics + OTLP** | `xai-grok-telemetry/src/` | 可观测性闭环 | 新增 `legion-telemetry` crate |
| **Leader-follower IPC 架构** | `xai-grok-shell/src/leader/` | 多客户端共享一个 agent 实例 | 对 Legion 的本地 CLI/IDE bridge 有价值 |
| **40+ 斜杠命令生态** | `xai-grok-pager/src/slash/commands/` | 终端效率 | 分阶段补齐 `/compact`、`/plan`、`/tasks`、`/context` 等 |
| **`grok agent stdio` ACP server 模式** | `xai-grok-shell/src/agent/app.rs` | IDE 集成标准入口 | 把 `legion-acp` 从 stub 落地为 stdio server |

---

## 5. 与现有 Legion gap 文档的对应关系

| 本文差距 | 已有 gap 文档 | 状态 |
|---|---|---|
| OS 级 sandbox | `03-shallow/sandbox-isolation.md` | P0，未关闭 |
| Approval 回路深化 | `03-shallow/approval-loop.md` | P0，未关闭 |
| Compaction 双阶段/prefire | `03-shallow/compaction.md` | P1，未关闭 |
| Memory 文件化/Dream | `03-shallow/memory-layers.md` | P1，Phase D（Team/Dreaming）待实施 |
| Plugin facade | `02-missing/plugin-facade.md` | P0，未关闭 |
| MCP OAuth/credential | `02-missing/mcp.md` | P1，已关闭但可深化 |
| Multi-agent | `02-missing/multi-agent.md` | P1，已关闭 |
| Prompt management | `03-shallow/prompt-management.md` | P1，已关闭 |
| Session resume | `03-shallow/session-resume.md` | P2，已关闭 |
| Channels 广度 | `04-breadth/channels.md` | P2，已关闭 |
| Providers 广度 | `04-breadth/providers.md` | P2，已关闭 |
| Tools P1/P2 | `04-breadth/tools-p1p2.md` | P2，已关闭 |
| Automation advanced | `04-breadth/automation-advanced.md` | P2，已关闭 |

**本文新增/尚未被现有 gap 覆盖的能力**：
- 终端 TUI 深度（屏幕模式、40+ 斜杠命令、Dashboard、主题、语音）
- Goal 编排与 plan mode
- Scheduler tools / `/loop`
- Workspace/VCS 集成（worktree、checkpoint、rewind、hunk tracker）
- Leader-follower IPC 架构
- `stdio` ACP server 模式（`legion-acp` 落地）
- Unified log / Mixpanel / Sentry / OTLP 产品遥测
- 用户级 session UI（fork/rewind/share/export/trace）
- 托管配置 / MDM / `legion setup`

---

## 6. 建议优先级

| 优先级 | 能力 | 理由 |
|---|---|---|
| **P0** | OS 级 sandbox | 安全地基；现有 gap 已标记 |
| **P0** | Approval 回路深化（rules/hooks/modes） | 现有 gap 已标记；Grok 提供完整参考实现 |
| **P1** | Workspace/VCS 抽象（至少 git status + checkpoint） | Legion 作为 coding agent gateway 必须具备代码库感知 |
| **P1** | Goal 编排 + Plan mode | 与现有 Task Flow DAG 互补，提升 agent 自规划能力 |
| **P1** | MCP OAuth / credential store | 企业 MCP 接入刚需 |
| **P1** | `legion agent stdio` ACP server | IDE 集成的标准形态；`legion-acp` 已 stub |
| **P2** | TUI 深度（主题、斜杠命令、Dashboard） | 体验提升，但非 gateway 核心 |
| **P2** | Unified log / product telemetry | 可观测性闭环 |
| **P2** | Markdown 记忆文件 + Dream | 用户可感知的记忆 |
| **P2** | Leader-follower IPC | 本地多客户端共享 agent，长期有价值 |

---

*文档位置：`docs/design/gaps/05-grok-cli-comparison.md`*
