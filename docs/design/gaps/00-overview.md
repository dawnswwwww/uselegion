# Legion 能力差距总览(Roadmap Index)

> 本文件是 `docs/design/gaps/` 的入口与路线图。它把"legion 相对 Claude Code + 自身 PRD 的差距"梳理成 **14 个可执行项**,给出优先级矩阵、依赖关系与三阶段路线图。
>
> 阅读顺序建议:先 [`01-guiding-principles.md`](./01-guiding-principles.md)(设计宪法)→ 本文件(全景与排序)→ 按需进入各类目子文档。

---

## 1. 背景:这份文档解决什么

legion 是一个 Rust 多通道 AI agent gateway,当前处于 **Phase 0 MVP 中后期**:Gateway→Channel→Runtime→Provider→Tools→Memory→Compaction 主链路端到端打通,核心数据路径(memory 检索、agent 循环、工具执行、provider 路由、channel 双向消息、cron 调度)都有非占位实现和测试。

但对照两个参考系,存在显著差距:

- **对照 Claude Code 泄露源码**(`claude-code-analysis/`):Claude Code 在 agent 执行**内核深度**上沉淀了大量工程防线(approval 回路、compact 熔断、sandbox 逃逸防护、session orphan 修复、四层 memory),legion 在这些"防线"上普遍缺失或骨架化。
- **对照 legion 自身 PRD**(`docs/design/agent-harness-prd.md`)与参考架构 OpenClaw(`docs/openclaw_raw/`):PRD 规划的扩展面(7 类插件、Skill、多通道、P1/P2 工具、原生客户端)完成度仅约 55%,OpenClaw 的连接面广度(27+ channel、60+ provider)legion 仅覆盖极小一部分。

本目录把这两类差距统一编码为 **15 个 gap**,分三大类目,每个 gap 独立成文并附设计方案。

**重要**:所有"现状"陈述以 **legion 源码为准**,不轻信 `AGENTS.md` 声明。涉及"借鉴 Claude Code"处,均指向 `claude-code-analysis/analysis/*.md` 的具体章节作为依据。

---

## 2. 差距全景:三大类目

| 类目 | 含义 | gap 数 | 严重度 |
|---|---|---|---|
| [`02-missing`](./02-missing/_index.md) | **完全缺失的子系统**:Claude Code 已深度实现,legion 零实现或仅有配置占位 | 4 | 最高 |
| [`03-shallow`](./03-shallow/_index.md) | **内核浅化**:legion 有基础实现,但缺 Claude Code 的工程化深度防线 | 7 | 高 |
| [`04-breadth`](./04-breadth/_index.md) | **生态广度不足**:对照 OpenClaw/PRD 的连接面、工具、自动化数量差距 | 4 | 中 |

### 2.1 完全缺失(`02-missing/`)
- [skills](./02-missing/skills.md) — ✅ 已完成:新增 `legion-skills` crate、frontmatter 解析、摘要注入、paths 条件触发、按需召回(关键词 + 可选轻量 LLM 选择器)、`legion skills list/reload`、plugin skill 来源(通过 `legion-plugin-sdk` 的 `PluginHandles`/`ManifestPlugin` 注入 runtime)。
- [mcp](./02-missing/mcp.md) — ✅ 已完成(**协议升级 2026-07-31**):`legion-mcp` crate 四种传输(stdio/http-streamable/sse/ws),协议版本协商链 2026-07-28→2024-11-05(含 2026-07-28 stateless 模式 + `server/discover` 回退 + `protocolVersion` pin),列表分页(cursor/nextCursor,100 页上限),resources/prompts 内省 API + `server_status()` 快照,`structuredContent`/`annotations`/`outputSchema` 透传,新配置 `protocolVersion`/`toolTimeoutMs`;`mcp__<server>__<tool>` 命名空间 + 默认 `Approval::Required` + 认证雪崩缓存 + 描述截断 + 并发限流 + session 重连 + Prometheus 指标沿用;CLI 扩为 `legion mcp add/remove/get/status/list/tools/reload`,新增 TUI `/mcp` slash 命令;剩余差距:OAuth flow、MRTR/elicitation、tools/list_changed 实时刷新、ttlMs/cacheScope、Tasks extension、配置热加载。
- [multi-agent](./02-missing/multi-agent.md) — ✅ **已完成(2026-07-11)**:`spawn_subagent`(Typed 独立上下文 / Fork 快照继承)+ `run_coordinator`(多阶段计划:同 phase 并行、phase 间按 `dependsOn` 串行、`{{results}}` 汇总注入)+ sidechain transcript + 权限收敛 + 深度/迭代/超时/并发防护 + 子 agent 审批默认拒绝;阶段 D Swarm 以 in-process 因地制宜形态落地(`SwarmManager`:命名 teammate + per-teammate mailbox + 跨轮历史续接,`swarm_spawn`/`swarm_send`/`swarm_status` 工具)
- [plugin-facade](./02-missing/plugin-facade.md) — 7 类插件仅 channel 可用,4 个 system plugin 是 stub

### 2.2 内核浅化(`03-shallow/`)
- [approval-loop](./03-shallow/approval-loop.md) — `Approval::Prompt≡Required`,主循环 `can_use_tool=None`
- [memory-layers](./03-shallow/memory-layers.md) — ✅ Phase C(2026-07-10):分层检索权重 + `recall` 去重(Phase A)+ 后台 auto_extract / secret scanning(Phase B)+ 查询时衰减 / keep-newest 合并(`legion memory merge`)/ 可选 LLM 召回 / 可配 `recall.limit` / 跨轮 `SurfacedStore` 去重(Phase C)已落地;Team/Dreaming(Phase D)待实施
- [compaction](./03-shallow/compaction.md) — 无熔断/状态复灌/PTL 防御/prompt cache
- [sandbox-isolation](./03-shallow/sandbox-isolation.md) — local backend 零隔离,无 namespace/seccomp
- [session-resume](./03-shallow/session-resume.md) — **Phase A+B+C 已落地(2026-07-11)**:`RunEvent::Compaction` 携带 `resume_head` + `load_for_resume` boundary 感知恢复(A);`transcript_repair.rs` orphan 修复 + 一致性检查(`sessions.orphanPolicy`)(B);`lite_read`/`list_session_summaries` 头读摘要 + TTL 归档(`sessions.ttlDays`/`archiveDir`,移动可恢复,gateway 启动执行)(C);sidechain(Phase D)随 multi-agent
- [prompt-management](./03-shallow/prompt-management.md) — **✅ 已落地(2026-07-11,Phase A+B+C)**:`SystemPromptBuilder` section 化 + bootstrap 补 `IDENTITY.md`/`HEARTBEAT.md`(A);`resolve_sections` override 优先级链(Override>Coordinator>Agent>Custom>Default,Append 挂末尾)+ per-agent `customSystemPrompt`/`appendSystemPrompt`/`outputStyle`/`language`(B);`promptDump.enabled`/`--dump-prompts` 落 JSONL(0600)+ `legion context <session>` 按段 token 表 + `cache_prefix_len`(C;provider cache breakpoint 接线留后续)
- [session-loop](./03-shallow/session-loop.md) — **Phase A 已实施(2026-07-16)**:local TUI 模式 `/loop` 不再强制依赖 gateway;`LocalDriver` 内嵌 `CronScheduler`,使用 session 私有 store(`<peer>.cron.jsonl`/`<peer>.tasks.jsonl`),job id 加 `session-cron-` 前缀,与 global gateway loop 分层管理

### 2.3 生态广度(`04-breadth/`)
- [channels](./04-breadth/channels.md) — **✅ 已完成(2026-07-11)**:Phase A=访问控制引擎真执行(`access.rs`,默认最小权限)+ BotLoopGuard;Phase B=Slack(Socket Mode)+ Discord(Gateway WS);Phase C=Lark(飞书长连接,手写 pbbp2 protobuf 帧编解码)+ Matrix(sync 长轮询,m.direct 判定 DM);收尾切片=`ChannelProvider` 加默认 no-op `send_typing`/`add_reaction`,Telegram 实现 sendChatAction/setMessageReaction,`route_inbound_to_runtime` 按 capabilities 门控 typing 循环(watch 停止)/👀 reaction,无能力的 channel 零开销不崩。WebChat media_send 复核为本就 pass-through。四个新 channel 均以 system plugin 包装注册,legion-channel lib 60 测试全过,**live 路径均无凭据未 E2E**;Phase D 桥接型(WhatsApp/iMessage)暂不承诺
- [providers](./04-breadth/providers.md) — **✅ 已完成(2026-07-11)**:Phase A=router 四项运维能力(retry 耗尽才 fallback、RPM/TPM 限流、`timeout_seconds` 真生效、CostTracker + `legion costs`);Phase B=Gemini(streamGenerateContent SSE)+ Ollama(NDJSON,本地免 auth);Phase C=prompt cache runtime 接线(`split_for_prompt_cache` 按 `cache_prefix_len` 切分 + Anthropic `cache_control`)+ `BedrockProvider`(ConverseStream + 手写 SigV4 签名 + event-stream 二进制帧解码/CRC32,`AuthProfile::AwsSigv4`);legion-provider 115 测试全过,**live API 均无凭据未 E2E**;遗留可选切片:Prometheus 指标、Bedrock 非流式/Guardrails;Phase D Azure/国内 provider 暂不承诺
- [tools-p1p2](./04-breadth/tools-p1p2.md) — ✅ **已完成(2026-07-11)**:Phase A=`session_status`/`sessions_list`/`sessions_history`(gateway 侧注册,仅当前 agent);Phase B=`agent_to_agent_send`(`AgentMessenger` fire-and-forget + `AgentConfig.allowFrom` 白名单,空=全拒,默认 Prompt)+ `image_generate`(`Provider::generate_image` 默认方法 + OpenAI `/images/generations` + 关键词预检 + b64 落盘 workspace,默认 Required);Phase C=`BrowserTool`(轻量 CDP 后端,外部 `tools.browser.cdpUrl` 端点,navigate/read 标 read-only,默认 Required)+ `TtsTool`(`Provider::synthesize_speech` 默认方法 + OpenAI `/audio/speech` + 产物落 workspace,默认 Off);共 51 新测试全绿,**live CDP/TTS API 未 E2E**;Phase D(canvas/video/nodes_*)暂不承诺
- [automation-advanced](./04-breadth/automation-advanced.md) — ✅ **已完成(2026-07-11)**:Phase A=Standing Orders(`agents.defaults/list[].standingOrders` 注入 cacheable prompt section,来源仅限配置);Phase B=Inferred Commitments(轻量 LLM 抽取 `{description, due}` → 一次性 `__at__` cron job,默认关);Phase C=Task Flow DAG(顶层 `flows` 声明式配置 + `FlowRunner` 按层并发执行 + abort/continue 失败策略,`flows.list/run` RPC + CLI)与 cron webhook(`__webhook__` sentinel + `POST /webhook/{id}` HMAC-SHA256 校验,404 不泄露存在性);共 34 新测试全绿(含 200 路径真实 Gateway 集成);Phase D(条件分支/revision)暂不承诺

---

## 3. 优先级矩阵

优先级判定标准见 [`01-guiding-principles.md`](./01-guiding-principles.md) §5。工作量:`S`≤3人日,`M`=1-2人周,`L`≥3人周。

| Gap | 类目 | 优先级 | 工作量 | 前置依赖 | 关联 PRD 章节 |
|---|---|---|---|---|---|
| [approval-loop](./03-shallow/approval-loop.md) | shallow | **P0** | M | — | §8 T4 |
| [sandbox-isolation](./03-shallow/sandbox-isolation.md) | shallow | **P0** | L | — | §8 T5/T6 |
| [plugin-facade](./02-missing/plugin-facade.md) | missing | **P0** | L | — | §10 |
| [skills](./02-missing/skills.md) | missing | **P0** | M | plugin-facade | §8 T1, §6 R3 |
| [mcp](./02-missing/mcp.md) | missing | P1 | L | plugin-facade | §10 PL1 |
| [memory-layers](./03-shallow/memory-layers.md) | shallow | P1 | L | — | §7 M2/M7 |
| [compaction](./03-shallow/compaction.md) | shallow | P1 | M | — | §6 R1/R6 |
| [multi-agent](./02-missing/multi-agent.md) | missing | P1 | L | — | §8 T3 |
| [prompt-management](./03-shallow/prompt-management.md) | shallow | P1 | M | skills | §6 R3 |
| [session-resume](./03-shallow/session-resume.md) | shallow | P2 | M | compaction | §15 D2 |
| [session-loop](./03-shallow/session-loop.md) | shallow | P2 | S-M | automation-advanced | §9 A3/A4 |
| [channels](./04-breadth/channels.md) | breadth | P2 | L | plugin-facade | §5 C4-C8 |
| [providers](./04-breadth/providers.md) | breadth | P2 | M | — | §6.5 P2/P6 |
| [tools-p1p2](./04-breadth/tools-p1p2.md) | breadth | P2 | L | plugin-facade | §8 T3 |
| [automation-advanced](./04-breadth/automation-advanced.md) | breadth | P2 | M | — | §9 A4/A5/A7 |

**优先级分布**:P0 × 4(安全地基 + 架构地基),P1 × 5(内核深度 + 扩展杠杆),P2 × 6(生态广度 + 健壮性 + UX 深度)。

**关键依赖链**:
```
plugin-facade ─┬─→ skills ─→ prompt-management
               ├─→ mcp
               └─→ tools-p1p2 / channels
compaction ─→ session-resume
automation-advanced ─→ session-loop
```
`plugin-facade` 是架构地基,解锁 skills/mcp/channels/tools-p1p2 的低成本落地,故列 P0。

---

## 4. 全局路线图(三阶段)

### Phase A — 安全与架构地基(P0,~6-8 人周)
目标:消除默认配置下的失控风险,并为所有扩展能力打下插件化地基。

1. **approval-loop**:工具审批人机回路 + 主循环挂载 `can_use_tool`。(安全关键,先做)
2. **sandbox-isolation**:local backend 接入 OS 原语隔离(namespace/seccomp/sandbox-exec)。(安全关键)
3. **plugin-facade**:补齐 7 类插件注册 API + 包格式 + 动态加载 + panic 隔离。(架构地基)
4. **skills**:基于 plugin-facade 落地 Skill 发现/注册/执行 + frontmatter。(高杠杆扩展)

**Phase A 出口标准**:默认配置下,任何工具执行都经过 approval gate 且可人工拦截;`exec` 在 local backend 有真隔离;第三方可写 channel/tool/skill 插件。

### Phase B — 内核深度与扩展杠杆(P1,~8-10 人周)
目标:补齐 Claude Code 的工程化防线,并接入 MCP/Multi-Agent 两大扩展杠杆。

1. **memory-layers**:分层记忆 + 自动决策 + 召回选择器。
2. **compaction**:Auto-compact 熔断 + 状态复灌 + PTL 防御 + prompt cache。
3. **mcp**:MCP 客户端(stdio/sse/http)+ 认证缓存 + 描述截断。
4. **multi-agent**:Coordinator 模式 + 子 agent 委派(基于现有 task runner)。
5. **prompt-management**:分层 section + override 优先级 + dump 可观测。

**Phase B 出口标准**:长会话稳定不失控(compact 熔断);compact 后能力不丢(状态复灌);可接入任意 MCP server;可派生子 agent 做研究/验证。

### Phase C — 生态广度与健壮性(P2,~8-12 人周,可并行)
目标:扩展连接面与工具生态,补齐 session 健壮性。

1. **channels**:Slack / WhatsApp / iMessage / Discord(基于 plugin-facade,优先 Slack)。
2. **providers**:Gemini / Bedrock / Ollama 原生 + 重试/cache/成本核算。
3. **tools-p1p2**:browser / canvas / image_generate / tts / agent_to_agent。
4. **automation-advanced**:Standing Orders / Inferred Commitments / Task Flow DAG。
5. **session-resume**:compact boundary 恢复 + orphan tool_result 修复。
6. **session-loop**:local TUI 进程级 `/loop`,与 gateway global loop 分层。

**Phase C 出口标准**:覆盖至少 5 个 channel、5 个 provider;多步任务可 DAG 编排;resume 能正确恢复 compact 后的会话。

> 三个 Phase 并非严格串行:Phase C 的 `session-resume` 仅依赖 Phase B 的 `compaction`,可在 Phase B 中后期插入;`channels`/`providers` 在 plugin-facade 完成后即可启动,不必等 Phase B 全部完成。

---

## 5. 阅读指南

### 5.1 按"我想做什么"导航

| 你的目标 | 先读 |
|---|---|
| 理解整体差距与排序 | 本文件 + [`01-guiding-principles.md`](./01-guiding-principles.md) |
| 修复某个安全风险 | [`03-shallow/approval-loop.md`](./03-shallow/approval-loop.md)、[`03-shallow/sandbox-isolation.md`](./03-shallow/sandbox-isolation.md) |
| 扩展 agent 能力(技能/外部工具) | [`02-missing/skills.md`](./02-missing/skills.md)、[`02-missing/mcp.md`](./02-missing/mcp.md) |
| 提升长会话稳定性 | [`03-shallow/compaction.md`](./03-shallow/compaction.md)、[`03-shallow/memory-layers.md`](./03-shallow/memory-layers.md) |
| 新增聊天渠道/模型商 | [`02-missing/plugin-facade.md`](./02-missing/plugin-facade.md)、[`04-breadth/channels.md`](./04-breadth/channels.md)、[`04-breadth/providers.md`](./04-breadth/providers.md) |
| 派生子 agent 并行任务 | [`02-missing/multi-agent.md`](./02-missing/multi-agent.md) |

### 5.2 每个 gap 文档的统一结构
所有 gap 文件遵循 9 节固定骨架(定义见 [`01-guiding-principles.md`](./01-guiding-principles.md) §6.2):

1. **元信息表** — 优先级 / 工作量 / 依赖 / 关联 PRD
2. **现状证据** — `file:line` 锚定
3. **设计目标** — 对照七条横切原则
4. **架构设计** — 模块职责、数据流
5. **接口设计** — Rust trait / struct / 配置 schema
6. **集成点** — 改动哪些现有 crate / 文件
7. **风险与权衡** — 借鉴 vs 因地制宜
8. **实现路线图** — 分阶段步骤
9. **验收标准** — 可自动化验证清单

### 5.3 如何使用这些方案
- 方案是**设计建议**,不是最终实现;实施时以最新源码为准复核集成点。
- 每个 gap 的"接口设计"给出 Rust 签名,是**起点**而非终点,实施时按实际约束调整。
- "验收标准"可直接转为 issue 的 acceptance criteria。

---

## 6. 与现有文档的关系 & 声明同步流程

### 6.1 文档关系
```
docs/design/agent-harness-prd.md   ← PRD:"要做什么"(功能规格)
        │
        ▼
docs/design/gaps/                  ← 本目录:"现有差距 + 怎么补"(本文件 + 16 方案)
        │
        ▼
crates/*/src/**.rs                 ← 实现(以源码为最终事实)
        │
        ▼
AGENTS.md                          ← 声明:"实现了什么"(须与源码同步)
```

### 6.2 声明同步流程
每完成一个 gap 的实施:
1. 更新 `AGENTS.md` 对应章节(新增能力、CLI 命令、配置项)。
2. 在本文件 §7 变更日志登记。
3. 若 gap 方案在实施中偏离设计,回填更新对应 gap 文档(保持文档与实现一致)。

### 6.3 与 Claude Code 分析的关系
本目录所有"借鉴 Claude Code"的论断,依据均在 `claude-code-analysis/analysis/`:
- Memory → `04-agent-memory.md`
- Skills → `04c-skills-implementation.md`
- Tool Call / approval → `04b-tool-call-implementation.md`
- MCP → `04d-mcp-implementation.md`
- Sandbox → `04e-sandbox-implementation.md`
- Context/Compaction → `04f-context-management.md`
- Prompt → `04g-prompt-management.md`
- Multi-Agent → `04h-multi-agent.md`
- Session → `04i-session-storage-resume.md`

> `claude-code-analysis/` 是独立 git 仓库,非 legion 工作区成员,仅供分析引用,不参与编译。

---

## 7. 变更日志

| 日期 | 变更 | 作者 |
|---|---|---|
| 2026-07-31 | MCP 协议升级:`legion-mcp` 新增 `version.rs` 协议协商链(`SUPPORTED_VERSIONS` = 2026-07-28/2025-11-25/2025-06-18/2025-03-26/2024-11-05,新→旧 `initialize` 回退,server 版本宽松采纳 + capabilities 存储,`protocolVersion` pin 跳链);2026-07-28 stateless 模式(无 `notifications/initialized`、`_meta` clientInfo+能力、`MCP-Protocol-Version`/`Mcp-Method`/`Mcp-Name` 头、-32601 → `server/discover` 回退);http 传输升级 Streamable HTTP(SSE 帧解析、`Mcp-Session-Id` 捕获重发仅限 2025-xx);tools/resources/prompts list 分页(100 页上限);内省 API `list_resources`/`read_resource`/`list_prompts`/`get_prompt` + `McpManager::server_status()`(UI 用,非 agent 工具);`McpToolDesc` 带 annotations/outputSchema,结果带 structuredContent;配置新增 `protocolVersion`/`toolTimeoutMs`(默认 60s);CLI 扩为 `legion mcp add/remove/get/status/list`(claude-code 对齐 flag)+ 新增 TUI `/mcp` slash 命令(配置编辑持久化 legion.json,重启生效);剩余差距(OAuth flow/MRTR/tools/list_changed/ttlMs/Tasks/热加载)记入 mcp gap §2;更新 mcp gap、overview、DEVLOG | agent |
| 2026-07-17 | subagent 预算护栏重构:`defaultMaxIterations` 默认 5 → `None`(字段改 `Option<usize>`)+ `defaultTimeoutMs` 默认 120s → 600s。对齐 Claude Code(内置 agent 均不设 maxTurns,仅 fork=200 保险丝)与 grok(`resolve_subagent_max_turns`:子声明优先、否则继承父,父默认 None);legion 主 agent `agents.defaults.maxIterations` 本就默认 None;`run_child` 仅在 `Some` 时调 `with_max_iterations`,None 时回落 runtime 自身上限,600s 墙钟超时成为唯一预算护栏;实测动机:46 个子 agent 中 14 个(30%)死于 5 次迭代上限(读设定+写4文件恰卡线),此前两轮亦全部撞 120s 超时;spawn schema/coordinator(Option 本就兼容)/swarm(None 传入)无需改动;配置测试与 multi-agent §6.5 更新 | agent |
| 2026-07-17 | subagent 可观测性三修:(1) `ProviderRouter::validate_model_ref`(别名解析 + provider 已注册校验),`RuntimeSubagentSpawner` 对显式 `model` 覆盖预检,非法值(如模型幻觉的 "default"/"")立即 Failed 并提示省略参数继承默认,不再白跑一轮子 agent;(2) `TelemetryClient::log_session_event` 写入时统一注入 RFC 3339 `ts` 字段,事件间隔/重叠可从 session-metrics.jsonl 直接重建;(3) 超时子 agent 改为逐事件 deadline(`collect` 内 `timeout_at`),保留截止前已收集事件流写 sidechain,不再丢成 0 字节;配套 `AgentRuntime::router_for`(`run()` 复用);新增测试 `validate_model_ref_checks_alias_and_provider`/`spawn_invalid_model_override_fails_fast`/`timed_out_child_keeps_partial_event_stream` + telemetry ts 断言 | agent |
| 2026-07-17 | `spawn_subagent` 放开并行:`SpawnSubagentTool::is_concurrency_safe` 翻为 true(原为 false 强制串行批次),同一轮内多个 spawn 调用进入同一并发批次并行执行,受 `subagents.maxConcurrent` 信号量限流,permit 等待以子 agent 超时为上界(防嵌套派生占满许可死锁);是否并行由模型按任务独立性自行决定;工具 description 增补"同轮多次调用即并行"说明;新增 registry 层测试 `same_turn_spawn_subagent_calls_run_concurrently` 验证分桶与 spawner 层测试 `spawn_permit_wait_times_out_as_concurrency_error`;更新 multi-agent gap §4.2 与 overview | agent |
| 2026-07-11 | multi-agent 阶段 D 落地(gap ✅ 收官,路线图 14/14 全绿):新建 `legion-runtime/src/swarm.rs`(`SwarmManager`:命名 teammate 上限 8 + per-teammate mailbox 容量 16;`supervise` 循环同锁 drain+Idle 判定无丢消息;跨轮历史续接截断 40 条;每轮经 `RuntimeSubagentSpawner` 驱动复用信号量/超时/sidechain);`run_child` 去 `inherit_history` 门控(Typed 也支持 history);`swarm_spawn`/`swarm_send`(默认 Prompt)/`swarm_status`(默认 Off)三工具 + `ToolContext.swarm` 照 messenger 链透传;gateway `set_swarm` 接线;18 新测试全绿,全量 27 suite 全绿;更新 multi-agent gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | automation-advanced Phase C 落地(gap ✅ 收官):`legion-core` 加 `TaskFlow`/`FlowStep`/`FlowFailurePolicy` + 顶层 `flows` 声明式配置;新建 `legion-automation/src/flow.rs`(`FlowRunner` 预校验 + 按层 `FuturesUnordered` 并发 + abort/continue 失败策略 + 循环检测,`FlowReport` 序列化);cron 加 `webhook_secret` + `__webhook__` sentinel + `verify_webhook_signature`(HMAC-SHA256 常量时间比较)+ `get_job`;Gateway `POST /webhook/{id}`(404 不泄露存在性/401/200 触发)+ `flows.list/run` RPC;CLI `legion flows list|run`、`cron add --webhook-secret`;19 新测试(含 webhook 200 路径真实 Gateway 集成),全量 27 suite 全绿;更新 automation-advanced gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | automation-advanced Phase B 落地:新建 `legion-runtime/src/commitments.rs`(`CommitmentExtractor` trait fire-and-forget,照 AgentMessenger 模式)+ `legion-automation/src/commitments.rs`(`LlmCommitmentExtractor`:轻量 LLM 抽取 `{description, due RFC3339}` → 一次性 `__at__` CronJob,id 前缀 `commitment:`,SecretScanner 过滤 + cooldown + 失败全吞);顶层 `commitments` 配置(默认 disabled);gateway cron store 创建提前,extractor 与 scheduler 共享实例;`legion commitments list` CLI;automation 加 legion-provider 依赖(无环);8 新测试全绿,全量 26 suite 全绿;live LLM 未 E2E;更新 automation-advanced gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | automation-advanced Phase A 落地:`legion-core` 新类型 `StandingOrder { id, instruction, enabled }` + `AgentDefaults.standingOrders`(全局)/`AgentConfig.standingOrders`(per-agent,scope 由声明位置表达,无 scope enum);`SectionId::StandingOrders`;`assemble_system_prompt(_report)` 加 `standing_orders` 参数,enabled 非空时注入单个 cacheable section(max_tokens 2000,custom Base 之后)+ tracing;agent_loop 合并全局在前传入;7 新测试全绿,全量 26 suite 全绿(756 passed);更新 automation-advanced gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | tools-p1p2 Phase C 落地(gap ✅ 收官):新建 `legion-tools/src/browser.rs`(`BrowserTool` CDP 轻量后端,`tools.browser.cdpUrl` 外部端点,navigate/read/screenshot,产物落 workspace,默认 Required,设计变更:不在 sandbox 内嵌)+ `legion-gateway/src/tts_tool.rs`(`TtsTool` + `Provider::synthesize_speech` 默认方法 + OpenAI `/audio/speech`,默认 Off);legion-tools 加 futures/tokio-tungstenite;21 新测试全绿,全量 26 suite 全绿;live CDP/TTS 未 E2E;Phase D 暂不承诺;更新 tools-p1p2 gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | tools-p1p2 Phase B 落地:`legion-runtime/src/messenger.rs`(`AgentMessenger` fire-and-forget + `MessengerError`),`ToolContext` 加 `messenger`(照 spawner 链);`AgentConfig.allowFrom`(空=全拒);gateway `RuntimeAgentMessenger`(`check_allowed` + 后台 a2a turn);`AgentToAgentSendTool`(默认 Prompt,self-send 拒绝);`Provider::generate_image` 默认方法 + OpenAI `/images/generations`;`legion-gateway/src/image_tool.rs`(关键词预检 + b64 落盘 workspace,默认 Required);顺手修 discord.rs let-chains(MSRV 1.86);15 新测试全绿,全量 26 suite 全绿;更新 tools-p1p2 gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | tools-p1p2 Phase A 落地:新建 `legion-gateway/src/session_tools.rs`(`session_status`/`sessions_list`/`sessions_history` 三 Tool 实现,仅当前 agent_id——跨 agent key 拒绝 + peerId `[A-Za-z0-9._-]` 白名单防穿越 + content 截 2000);`SessionStore` 加 `stats`/`transcript_messages`;`CoreToolRegistry` 加 `register` 钩子(重名 warn 不覆盖),gateway 在 AgentRuntime 构建前注册并共享 `Arc<SessionStore>`;15 新测试全绿,全量 26 suite 全绿;更新 tools-p1p2 gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | providers gap ✅ 收官:runtime 侧 prompt cache 接线(`BuiltPrompt::split_for_prompt_cache` 按 `cache_prefix_len` 切 system blocks + cache_breakpoint,`use_prompt_cache` 门控,Anthropic `cache_control` 早有);`BedrockProvider`(ConverseStream)落地——新建 `sigv4.rs`(SigV4 纯函数签名 + Hinnant 日期算法,签名 known-answer 外部独立计算)与 `eventstream.rs`(手写 CRC32 + 帧解码),`AuthProfile::AwsSigv4` 新 variant,router 注册 bedrock kind;workspace 加 sha2/hmac;39 新测试,legion-provider 115 测试全过,全量 26 suite 全绿;live AWS 无凭据未 E2E;更新 providers gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | providers Phase B 落地:新建 `legion-provider/src/gemini.rs`(streamGenerateContent SSE + 纯函数 `to_gemini_request` 请求映射 + functionCall + batchEmbedContents + 静态模型目录)与 `ollama.rs`(/api/chat NDJSON 行缓冲流 + /api/embed + /api/tags `list_models`,本地免 auth);router `from_configs` 注册 gemini/ollama kind;27 新测试(wiremock),legion-provider 79 测试全过,全量 26 suite 全绿;live API 无凭据未 E2E;更新 providers gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | providers Phase A 落地:legion-core `ProviderConfig` 加 `retry`/`rateLimit`,`ModelsConfig` 加 `costs`;新建 `legion-provider/src/ops.rs`(`is_retryable`/`RetryPolicy`/`RateLimiter` token bucket/`CostTracker` write-through JSON/`track_chat_cost` unfold 流包装,tiktoken cl100k 估算);router 每 candidate 走 限流→retry 循环(耗尽才 fallback)→timeout 包裹(`timeout_seconds` 真生效,Timeout 判 retryable)→tracing+cost;新错误 RateLimited/Timeout;gateway 传 `~/.legion/agents/<agent>/costs.json`;CLI `legion costs` 跨 agent 聚合报表;26 新测试全绿(含 wiremock 429/500 分类、pause 时钟限流、timeout 生效、cost 流);Prometheus 指标留后续;更新 providers gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | channels gap ✅ 收官:`ChannelProvider` trait 加默认 no-op `send_typing`/`add_reaction`;Telegram 实现 sendChatAction/setMessageReaction(capabilities 翻 true);`route_inbound_to_runtime` 按 capabilities 门控(typing 4s 循环 watch 停止 + 👀 reaction,false 则零开销),provider 查找两次合并为一次;WebChat media_send 复核本就 pass-through;8 新测试(wiremock 6 + 路由门控 2),legion-channel lib 60 测试,全量 26 suite 全绿;gap 状态改 ✅(Phase D 桥接型暂不承诺);更新 channels gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | channels Phase C 落地:新建 `legion-channel/src/lark.rs`(飞书长连接 WS:`/callback/ws/endpoint` 取 URL + 手写 pbbp2.Frame protobuf 编解码 + ping/pong + `im.message.receive_v1` 解析 + DATA ack 防重投 + tenant_access_token 缓存发送,gzip 帧暂丢弃)与 `matrix.rs`(sync 长轮询 + whoami + `parse_sync_response` 解析 m.text/image/file + m.direct 判定 DM + PUT send);`LarkPlugin`/`MatrixPlugin` 注册,gateway 按 `channels.lark/matrix` 启停;16 纯函数单测全过,legion-channel lib 52 测试,全量 26 suite 全绿;live 路径无凭据未 E2E;更新 channels gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | channels Phase B 落地:新建 `legion-channel/src/slack.rs`(Socket Mode:`apps.connections.open` + 纯函数 envelope/message 解析 + ack/disconnect 重连,`chat.postMessage` 发送)与 `discord.rs`(Gateway WS:HELLO/IDENTIFY intents=37377/心跳 op1/READY 存 bot id/MESSAGE_CREATE 解析,op7/op9 重连不 RESUME,`POST /channels/{id}/messages` 发送);`plugins.rs` 加 `SlackPlugin`/`DiscordPlugin` 包装注册进 PluginRegistry,`SystemPlugins` 加字段,gateway 按 `channels.slack/discord` 启停;14 纯函数单测全过,live 网络路径无凭据未 E2E;更新 channels gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | channels Phase A 落地:新建 `legion-channel/src/access.rs`(`AccessPolicy`/`DmPolicy` open-allowlist-pairing/`GroupPolicy` requireMention 默认 true + groups allowlist;`evaluate`/`policy_for` 从 `channels.<id>.access` 解析,缺省最小权限;`BotLoopGuard` 按 (channel,peer) 跟踪 outbound 回复,窗口内超限拒 inbound);`route_inbound_to_runtime` 加 `bot_guard` 参并在 approval 处理后强制访问评估(Deny/RequireMention/BotLoop 全 tracing),回复发送后 `record_outbound`;gateway 接线共享 guard(60s/5 次);**行为变化**:无 `access` 配置 DM 默认拒绝(原"假功能"安全修复),WS `agent` 路径不受影响;更新 channels gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | session-resume Phase C 落地:`SessionStore::lite_read`/`list_session_summaries`(头读 64 KiB 提取首条 prompt + `truncated` 标记,`SessionSummary`);`archive_expired`(按最后 entry 时间戳、只读文件尾 8 KiB,移动归档至 `<archiveDir>/agents/<agent>/sessions/`,可移回恢复);`SessionsConfig` 加 `liteReadBufferBytes`/`ttlDays`/`archiveDir`;gateway `start`/`start_bound` 启动时一次性 TTL 归档;更新 session-resume gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | session-resume Phase B 落地:新建 `legion-gateway/src/transcript_repair.rs`(`recover_orphaned_tool_results`:orphan result 双策略丢弃,orphan use 按 `OrphanPolicy` synthesize 补 `[interrupted]` 占位 / dropOrphan 剔除 call 与空 assistant;`check_resume_consistency` 只读统计 + `ConsistencyReport`);legion-core 加 `SessionsConfig`(`sessions.orphanPolicy`,默认 synthesize);`websocket.rs` resume 加载后自动修复并非 clean 时 warn drift;更新 session-resume gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | session-resume Phase A 落地:`RunEvent::Compaction` 加 `resume_head`(compacted 历史去掉 leading system prompt,含 summary/reattachments/kept tail);gateway 在 compaction 时把 `resume_head` 追加到 boundary 之后(transcript 结构对齐 Claude Code post-compaction 布局);`SessionStore::load_for_resume`(`load_entries` 重构 + 最后 boundary 定位 + 无 boundary 退化全量);`websocket.rs` resume 改用 `load_for_resume`;更新 session-resume gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | prompt-management Phase B+C 落地:`resolve_sections` 优先级链(`source_rank` Override5>Coordinator4>Agent3>Custom2>Default1,同 id 取最高、平局留先注册者,Append 全部保留移末尾);`AgentConfig` 加 `customSystemPrompt`/`appendSystemPrompt`/`outputStyle`/`language`,`assemble_system_prompt_report` 加 `agent_prompt` 参,`agent_loop` 按 agent_id 查表注入;`BuiltPrompt` 加 `section_sources`/`cache_prefix_len` + `write_dump`(JSONL append、0600);顶层 `promptDump.enabled` 配置;`RunRequest.dump_prompts` + gateway `dumpPrompts` 参数 + `legion agent --dump-prompts`;新增 `legion context <session>` 子命令(本地读 dump 最后一行列按段 token 表);更新 prompt-management gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | prompt-management Phase A 落地:新建 `legion-runtime/src/prompt.rs`(`SectionId`/`SectionSource`/`PromptSection`/`SystemPromptBuilder`/`BuiltPrompt`;注册序拼接、`max_tokens` line-wise 截断 + `truncated` 报告、按段 token 报告);`assemble_system_prompt` 重构为 builder 注册并保持逐字兼容,新增 `assemble_system_prompt_report`;bootstrap 补 `IDENTITY.md`/`HEARTBEAT.md`;recalled memory 段标 `uncached`;更新 prompt-management gap、overview §2.2、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | multi-agent Phase C 落地:新增 `legion-runtime/src/coordinator.rs`(`CoordinatorPlan`/`CoordinatorPhase`/`CoordinatorTask` serde 解析 + 结构校验 + `run_coordinator_plan` 执行器:同 phase spawn-then-join 并行、phase 间串行、`{{results}}` 汇总注入)+ `CoordinatorReport::render`;新增 `run_coordinator` 工具(plan JSON 输入、逐 task 复用 `validate_tool_subset` 收敛校验);`SubagentHandle::from_receiver` 改 pub 供外部 spawner 实现;入口选工具而非 `legion agent --plan` CLI(避免 gateway/CLI 改动);更新 multi-agent gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | multi-agent Phase B 落地:`SubagentKind::Fork` + `SubagentRequest.parent_agent_id/history`;`ToolContext` 加 `parent_history: Option<Arc<Vec<ChatMessage>>>`,`run_loop` 每个 tool batch 前取快照经 pipeline 下发;`spawn_subagent` 支持 `kind: "typed"|"fork"`(`required` 降为 `["prompt"]`,fork 免 `agent_type`);子 agent required-approval 工具默认拒绝(已有 `interactive=false` fail-closed + 新增 spawner 层集成测试);更新 multi-agent gap、overview、DEVLOG 与 AGENTS.md | agent |
| 2026-07-11 | multi-agent Phase A 落地:`SubagentConfig`(`maxConcurrent=4`/`defaultTimeoutMs=120000`/`defaultMaxIterations=5`/`maxDepth=2`);`RunRequest` 加 `depth`/`allowed_tools`/`max_iterations`;新增 `legion-runtime/src/subagent.rs`(`SubagentSpawner` trait + `RuntimeSubagentSpawner`:Typed 独立上下文 + `tokio::spawn` + oneshot + `tokio::time::timeout` + sidechain `sessions/<child>/subagent-<handle>.jsonl`);`ToolContext` 加 `allowed_tools`/`spawner`/`depth`;`AgentRuntime` 晚绑定 `Mutex<Option<Arc<dyn SubagentSpawner>>>` + `set_spawner` 破构造循环;`run_loop` 按 `allowed_tools` 过滤 definitions + 越界调用结构化拒绝(不执行);新增 `spawn_subagent` 工具(`resolve_child_allowed` 强制子集 + 拒 `mcp__`);gateway 注入 spawner;顺手修 `task_runner` session key 8 段→7 段;更新 multi-agent gap、overview §2.1、DEVLOG 与 AGENTS.md | agent |
| 2026-07-10 | memory-layers Phase C:`memory.recall`/`decay`/`merge` 配置;`LlmRecallSelector`(可选 LLM 重排)+ `SurfacedStore`(跨轮去重持久化);backend 查询时按 `created_at` 对 episodic 乘 `decay_factor` + keep-newest `decay_and_merge`(经新 `legion memory merge` 触发);`assemble_system_prompt` 改走运行时注入的 `recalled`;更新 memory-layers gap 文档、DEVLOG 与 AGENTS.md | agent |
| 2026-07-10 | memory-layers Phase B:后台 `auto_extract`(turn 结束 `tokio::spawn` + cheap model 抽事实 + 内容哈希 id 去重)+ `SecretScanner`(命中即丢弃);`memory.autoExtract` 配置(默认关闭);更新 memory-layers gap 文档、DEVLOG 与 AGENTS.md | agent |
| 2026-07-10 | memory-layers Phase A:`MemoryKind` 分层检索权重(Working 1.0 / Episodic 0.75 / Semantic 0.55)+ `MemoryBackend::recall` 默认实现(over-fetch / 加权 / already_surfaced+recent_tools 去重 / top-5);`context.rs` 改走 `recall`;更新 memory-layers gap 文档、DEVLOG 与 AGENTS.md | agent |
| 2026-07-10 | skills 默认路径扩展:支持 `~/.agent/skills`、`<workspace>/.agent/skills` 并与 `~/.legion/skills` 合并加载;更新 AGENTS.md 与 skills gap 文档 | agent |
| 2026-07-10 | skills Phase C 完成:plugin skill 来源落地(`PluginHandles`/`Capability::Skill`/`ManifestPlugin::skills`/`PluginRegistry::skills`/`AgentRuntime::with_plugin_skills`);gateway 统一初始化并注入;更新 skills gap 文档与 AGENTS.md | agent |
| 2026-07-10 | skills Phase A+B 完成并新增 `legion skills list/reload` CLI:prompt 注入、paths 触发、按需召回、token 截断、CLI 扫描/验证;更新矩阵描述与 AGENTS.md | agent |
| 2026-07-09 | 初始化差距文档体系:14 个 gap 分类、优先级矩阵、三阶段路线图 | gap analysis |

---

*本文档为活文档,优先级与路线图随实施进展滚动更新。任何 gap 完成或新增,先更新本文件 §3 矩阵与 §4 路线图,再更新对应子文档。*
