# Gap:Multi-Agent / Subagents(无执行路径)

| 字段 | 值 |
|---|---|
| 类目 | [02-missing](./_index.md)(完全缺失) |
| 优先级 | P1(高杠杆扩展) |
| 工作量 | L(≥3 人周) |
| 前置依赖 | 无(复用现有 task_runner 与 ACP harness) |
| 关联 PRD | `agent-harness-prd.md` §8 T3(`subagent_spawn` 工具)、`docs/openclaw_raw/concepts/multi-agent.md` |
| 关联分析 | `claude-code-analysis/analysis/04h-multi-agent.md` |
| 状态 | ✅ 已实施(Phase A+B+C+D,2026-07-11,见 DEVLOG;Swarm 为 in-process 因地制宜版) |

---

## 1. 现状证据

- **枚举存在但无构造点**:`legion-automation/src/tasks.rs:20` 定义 `TaskKind::Subagent`,但 `grep TaskKind::Subagent` **全仓库无任何构造代码**——没有地方会创建一个 Subagent 任务。
- **无派生工具**:`legion-tools/src/registry.rs` 无 `spawn_subagent` / `agent_to_agent_send` 工具(PRD T3 明确规划但未实现)。
- **无委派机制**:`legion-runtime/src/agent_loop.rs` 的主循环中,没有任何"派生子 agent、等待结果、回填"的路径。
- **多 agent 仅停留在配置层**:`AgentRuntime` 支持 per-agent-id 的 provider router(`agent_loop.rs:52-60`),但只是"同一 runtime 按不同 agent_id 选 workspace/router",**不是真正的运行时委派**。
- **已有可复用基础设施**:`legion-automation/src/task_runner.rs` 有带 `depends_on` 依赖解析的后台执行器;`legion-acp` 有外部 harness 桥接(子进程 JSON-RPC)。这两者是 multi-agent 落地的地基,但当前未被用于"进程内子 agent"。

**结论**:legion 能跑单 agent 单线,但**无法派生、并行、委派**。复杂任务(研究→综合→实现→验证)只能串行由主 agent 完成。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:agent 在循环中通过 `spawn_subagent` 工具派生子 agent;Coordinator 模式可声明多阶段计划。
- **P2 安全**:子 agent 的 `allowed_tools` 是父 agent 工具集的**子集**(权限收敛,不放大);子 agent 审批回流父 agent 的 approval gate。
- **P3 增量**:无 spawn 调用时,主循环行为不变。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:子 agent 派生/完成/失败产生 `tracing` span 嵌套;sidechain transcript 可追溯。
- **P6 失败显式**:子 agent 超时/错误/被中止分类回流;父 agent 收到结构化失败而非静默。
- **P7 测试**:fork 继承上下文、权限收敛、Coordinator 依赖、sidechain 隔离各有测试。

---

## 3. 架构设计

### 3.1 借鉴 Claude Code 三模型,因地制宜选两模型

Claude Code 04h 有三套并存:(1) 普通 subagent;(2) Coordinator Mode;(3) Swarm Teammates(mailbox + 共享任务面)。

**legion 取舍**:
- **普通 subagent(Fork / Typed)** → **实现**(Phase B)。这是基础,`spawn_subagent` 工具 + 子 agent 运行时复用。
- **Coordinator Mode** → **实现**(Phase B)。复用现有 `task_runner` 的 `depends_on`,把 Research→Synthesis→Implementation→Verification 映射为 task 依赖图,无需新调度器。
- **Swarm Teammates(mailbox)** → **延后**(P2)。legion 当前无多进程 teammate 形态,mailbox 价值有限,列为 Phase C 研究。

### 3.2 Fork vs Typed 子 agent

| 类型 | 行为 | 适用 |
|---|---|---|
| **Fork**(隐式) | 继承父 agent 完整上下文 + workspace + router,在父上下文基础上继续 | "帮我看下这个文件再继续" |
| **Typed**(显式 `agent_type`) | 独立上下文(仅注入 system prompt + 任务描述),不继承父历史 | "派一个 researcher 去搜集,结果回传" |

### 3.3 数据流

```
父 agent_loop
   │  调用 spawn_subagent 工具
   ▼
SubagentSpawner.spawn(req)
   │  ├─ Fork: clone 父 context + workspace + router
   │  └─ Typed: 新建 context,注入 system_prompt + prompt
   ▼
tokio::spawn(子 agent_loop)  ──→ sidechain transcript: sessions/<parent>/subagents/agent-<id>.jsonl
   │
   ▼  oneshot channel 回流
SubagentResult { text, tool_calls, transcript_path }
   ▼
作为 tool_result 回填父 agent 循环
```

### 3.4 Coordinator 模式(基于 task_runner)

```
CoordinatorPlan { phases: [Phase] }
Phase {
   name: "research",
   tasks: [SubagentRequest, ...],   // 同 phase 内可并行
   depends_on: [],                  // 依赖的前置 phase
}
```
用 `legion-automation/src/task_runner.rs:108-134` 的依赖解析执行:同 phase 任务并行,phase 间按依赖串行。Coordinator agent 收集各 phase 结果做综合。

---

## 4. 接口设计(Rust)

### 4.1 子 agent 派生(`legion-runtime`)

```rust
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum SubagentKind {
    Fork,                  // 继承父上下文
    Typed(String),         // 指定 agent_type 的独立上下文
}

#[derive(Debug, Clone)]
pub struct SubagentRequest {
    pub kind: SubagentKind,
    pub prompt: String,
    pub model: Option<String>,                 // 覆盖 router 默认模型
    pub allowed_tools: Vec<String>,            // 必须是父工具集子集(权限收敛)
    pub parent_session: SessionKey,
    pub parent_context_summary: Option<String>,// Fork 时注入的父上下文摘要
    pub max_iterations: usize,                 // 防失控(默认 5)
    pub timeout: std::time::Duration,          // 默认 120s
}

#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub handle_id: String,
    pub text: String,                          // 最终文本(回填父 tool_result)
    pub tool_call_count: usize,
    pub transcript_path: std::path::PathBuf,   // sidechain 文件
    pub status: SubagentStatus,
}

#[derive(Debug, Clone)]
pub enum SubagentStatus { Completed, Failed(String), TimedOut, Aborted }

#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    async fn spawn(&self, req: SubagentRequest) -> Result<SubagentHandle, SubagentError>;
}

pub struct SubagentHandle {
    pub id: String,
    rx: tokio::sync::oneshot::Receiver<SubagentResult>,
}
impl SubagentHandle {
    pub async fn join(self) -> Result<SubagentResult, SubagentError>;
}
```

### 4.2 spawn_subagent 工具(`legion-tools`)

```rust
pub struct SpawnSubagentTool { spawner: Arc<dyn SubagentSpawner> }

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str { "spawn_subagent" }
    fn is_concurrency_safe(&self) -> bool { false }   // 阻塞型,串行
    async fn call(&self, input: Value) -> Result<ToolOutput, ToolError> {
        let req: SubagentRequest = serde_json::from_value(input)?;
        // 权限收敛校验:allowed_tools 必须是当前 agent 工具集子集
        validate_tool_subset(&req.allowed_tools, &self.available_tools)?;
        let handle = self.spawner.spawn(req).await?;
        let result = handle.join().await?;            // 阻塞等待(可被父循环中断)
        Ok(ToolOutput::Text(result.text))
    }
}
```

### 4.3 Coordinator 计划(`legion-automation` 复用)

```rust
pub struct CoordinatorPlan { pub phases: Vec<Phase> }

#[derive(Debug, Clone)]
pub struct Phase {
    pub name: String,
    pub tasks: Vec<SubagentRequest>,
    pub depends_on: Vec<usize>,   // phase 索引
}
// 执行:复用 task_runner.rs 的依赖解析
//   - 同 phase 内 tasks 并行 spawn + join
//   - phase 间按 depends_on 拓扑串行
//   - 每阶段结果汇总给 Coordinator agent
```

### 4.4 sidechain transcript(`legion-gateway/src/session_store.rs`)

```rust
// 现有: sessions/<peer_id>.jsonl (主链)
// 新增: sessions/<peer_id>/subagents/agent-<handle_id>.jsonl (子链)
impl SessionStore {
    pub fn subagent_path(&self, parent: &SessionKey, handle_id: &str) -> PathBuf;
    pub fn append_subagent(&self, parent: &SessionKey, handle_id: &str, entry: TranscriptEntry) -> Result<()>;
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-runtime/src/agent_loop.rs` | 新增 `SubagentSpawner` 持有者;主循环通过 `spawn_subagent` 工具触发派生。子 agent_loop 复用 `AgentRuntime::run`。 |
| `legion-tools/src/registry.rs` | 注册 `spawn_subagent` 工具(注入 spawner 引用)。 |
| `legion-gateway/src/session_store.rs:1-191` | 新增 sidechain 路径与 append(不混入主链,借鉴 Claude Code subagent sidechain)。 |
| `legion-automation/src/task_runner.rs` | Coordinator 模式复用其依赖解析(Phase 间拓扑排序)。 |
| `legion-core/src/config.rs` | 新增 `subagents: SubagentConfig { maxConcurrent, defaultTimeoutMs, defaultMaxIterations }`。 |

---

## 6. 风险与权衡

### 6.1 权限收敛(安全核心)
借鉴 Claude Code:子 agent 的 `allowed_tools` 必须是父工具集**子集**,防止"派一个权限更小的子 agent 却偷偷授予更大权限"。`spawn_subagent` 工具 call 时强制校验。MCP 工具默认不传给子 agent(除非显式声明)。

### 6.2 审批回流
Claude Code 04h §9:in-process teammate 的权限请求回流 leader 的 confirm queue。legion 类似——子 agent 遇到 `approval: required` 工具时,请求回流父 agent 的 approval gate(或直接拒绝,取决于配置)。**Phase B 默认:子 agent 的 required-approval 工具直接拒绝**(避免无人值守的子 agent 等待人类),可配置改为回流。

### 6.3 Fork 的上下文继承与 token 成本
Fork 继承父上下文可能很大。**缓解**:Fork 时注入"父上下文摘要"(走 compaction 的 summary 路径)而非全文,除非显式要求。这与 compaction gap 协同。

### 6.4 因地制宜:并发原语
Claude Code 用 `AsyncLocalStorage`(TS);legion 用 `tokio::spawn` + `oneshot` channel + `Arc` 共享 router,天然异步隔离,无需额外上下文传播机制。

### 6.5 失控防护
子 agent 有独立 `max_iterations`(默认 5,远小于主循环 10)+ `timeout`(默认 120s),防止子 agent 无限循环拖垮 Gateway。超时 → `SubagentStatus::TimedOut` 回流。

### 6.6 Swarm Teammates 的落地形态(2026-07-11 更新)
Claude Code 的 Swarm 依赖 tmux/iTerm2 多进程 + mailbox 文件通信。legion 是单 Gateway 进程,**阶段 D 以 in-process 形态落地**:命名 teammate = 进程内后台 agent(由 `RuntimeSubagentSpawner` 驱动每轮),mailbox = per-teammate 内存队列(`SwarmManager`),跨轮历史续接(截断 40 条)。多进程 teammate 形态若未来出现(如移动端 node),可在 `SwarmManager` 外加进程适配层,mailbox 语义不变。

---

## 7. 实现路线图

### 阶段 A(Phase B,~1.5 人周):Typed 子 agent + spawn_subagent 工具 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `SubagentSpawner` trait + Typed 实现(新建独立 context,复用 `AgentRuntime::run`)。✅
2. `spawn_subagent` 工具 + 权限收敛校验。✅
3. sidechain transcript 存储(`sessions/<child>/subagent-<handle>.jsonl`,不混入主链)。✅
4. 超时 + max_iterations + 深度(`max_depth`) + 并发(semaphore)防护。✅
5. **验收**:agent 调用 `spawn_subagent` 派生 Typed 子 agent,结果回填主循环;子 agent transcript 在 sidechain 文件。✅(`spawn_typed_completes_with_child_text` / `allowed_tools_denies_calls_outside_subset` / `spawn_depth_limit_rejected` / `spawn_timeout_yields_timed_out` / `resolve_child_allowed_*`)

> 实现相对原设计的微调:`AgentRuntime` 用 `Mutex<Option<Arc<dyn SubagentSpawner>>>` 晚绑定 + `set_spawner` 打破构造循环(runtime 先于 spawner 构造);委派 seam 走工具驱动,spawner 经 `ToolContext.spawner` 下发而非工具持有。

### 阶段 B(Phase B,~1 人周):Fork 子 agent + 审批回流 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. Fork 实现(继承父 context + workspace + router):`SubagentKind::Fork` + `SubagentRequest.history/parent_agent_id`;`run_loop` 在每个 tool batch 前取 `Arc<Vec<ChatMessage>>` 快照,经 `ToolContext.parent_history` 下发,`spawn_subagent {kind:"fork"}` 时作为 child `RunRequest.history`。✅
2. 子 agent required-approval 工具的处理:child run `with_interactive(false)`,`ApprovalGate` 对 unattended 请求 fail-closed 立即拒绝(`tool 'x' approval denied`),**默认拒绝**已生效;"可配置回流"留后续。✅
3. **验收**:Fork 子 agent 继承父上下文(`fork_child_inherits_parent_history` / `tool_receives_parent_history_snapshot_for_fork`);required-approval 默认拒绝(`child_required_approval_tool_is_denied_unattended`)。✅

### 阶段 C(Phase B 尾,~0.5 人周):Coordinator 模式 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `CoordinatorPlan`(`legion-runtime/src/coordinator.rs`):serde camelCase 解析 + 结构校验(phase 名唯一非空、tasks 非空、`depends_on` 必须指向更早声明的 phase——声明序即拓扑序,天然无环)。✅
2. 执行器 `run_coordinator_plan`:同 phase tasks 全部 spawn 后逐个 join(真并行,受 spawner semaphore 限流),phase 间串行;`{{results}}` 注入此前全部 phase 的渲染结果。✅
3. 入口 = `run_coordinator` 工具(而非 CLI):复用 `ToolContext.spawner` seam,零 gateway/CLI 改动;每个 task 的 `allowedTools` 复用 `validate_tool_subset`(子集 + 拒 `mcp__`)。✅
4. **验收**:research 两任务并行 → synthesis 依赖串行且收到汇总(`plan_runs_phase_tasks_concurrently_then_serializes_phases` / `run_coordinator_executes_plan_and_returns_report`)。✅

### 阶段 D(Phase C,研究):Swarm Teammates — ✅ 已落地(2026-07-11,见 DEVLOG,in-process 因地制宜版)
1. `legion-runtime/src/swarm.rs`(新建):`SwarmManager`——命名 teammate(`^[A-Za-z0-9._-]{1,32}$`,默认上限 8)+ per-teammate mailbox(默认容量 16)。**不做多进程/tmux**:teammate 是进程内后台 agent,每轮由现有 `RuntimeSubagentSpawner` 驱动(信号量/超时/sidechain 免费获得)。✅
2. **mailbox 驱动多轮**:`supervise` 循环——每轮结束在同一把锁内 drain 邮箱 + 判定 Idle(无丢消息不变量:send 要么被本轮 drain 捞走,要么看到 Idle 唤醒新 supervisor);`std::sync::Mutex` 锁内零 await。teammate 跨轮**历史续接**(每轮 `history.push(user/assistant)`,截断保留最后 40 条)——配套 `run_child` 去掉 `inherit_history` 门控,Typed 也支持 history(spawn_subagent Typed 传空 history,行为不变)。✅
3. 工具(legion-tools):`swarm_spawn`(默认 Prompt,agentType 缺省=父 agent,allowedTools 复用 `resolve_child_allowed` 权限收敛)/`swarm_send`(默认 Prompt)/`swarm_status`(默认 Off,read-only);`ToolContext.swarm` 照 messenger 链全线透传(7+ 构造点);gateway 在 `set_spawner` 同位置 `set_swarm`。✅
4. **验收**:18 个新测试(swarm 10:spawn→Idle/mailbox 唤醒/Running 时入队不重复唤醒/重名/非法名 32 字符边界/满员/邮箱满/未知 teammate/历史续接/render_mailbox 格式;subagent 1:Typed+history 端到端;tools 6:未接线/缺参/mcp__ 拒绝/roundtrip;registry 1);全量 27 suite 全绿,clippy/fmt 干净。

---

## 8. 验收标准

- [x] agent 可通过 `spawn_subagent` 工具派生子 agent,结果作为 tool_result 回填。(Phase A)
- [x] Typed 子 agent 有独立上下文(Phase A ✅),Fork 子 agent 继承父上下文(Phase B ✅,快照经 `ToolContext.parent_history` 下发)。
- [x] 子 agent `allowed_tools` 是父工具集子集,越界请求被拒(权限收敛测试)。(Phase A)
- [x] 子 agent 默认 `max_iterations=5`、`timeout=120s`,超限返回 `TimedOut`(失控防护测试)。(Phase A)
- [x] 子 agent transcript 写入 sidechain 文件,不混入主链(隔离测试)。(Phase A)
- [x] Coordinator 多阶段计划:同阶段并行、阶段间串行(依赖测试)。(Phase C,2026-07-11)
- [x] 子 agent required-approval 工具默认拒绝(安全测试:child `with_interactive(false)` + gate fail-closed;"可配置回流"留后续)。(Phase B)
- [x] 派生/完成/失败全程嵌套 `tracing` span(`run_child` 的 `info_span!("subagent")`)。(Phase A)
- [x] 无 spawn 调用时主循环行为不变(回归:`allowed_tools=None` 走原路径,`tool_pipeline` 既有测试全绿)。(Phase A)
- [x] `AGENTS.md` 新增 Multi-Agent 章节。(Phase A)

---

*上一篇:[`mcp.md`](./mcp.md) · 返回类目:[`_index.md`](./_index.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
