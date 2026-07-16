# Gap:Tool 审批无人机回路(安全关键)

> **实施状态**:Part 1 + Part 2 已完成(循环 1+2+3:`ApprovalGate` + `Permission::Prompt` 三态 + 异步 `CanUseToolFn` + `execute_tool_call` 接 gate + `agent_loop` 真实挂载 + `Policy`/`Approval` 上移至 `legion-runtime` + `RunRequest.interactive/sender/approval_gate` + channel `ChannelApprovalNotifier` + gateway `ApprovalQueueRegistry` 回流),见 [`docs/DEVLOG.md`](../../../DEVLOG.md) 2026-07-09。Phase C(`pre_tool`/`post_tool` hooks、审计日志)待后续切片实施。

| 字段 | 值 |
|---|---|
| 类目 | [03-shallow](./_index.md)(内核浅化) |
| 优先级 | **P0**(安全关键,最高优先级) |
| 工作量 | M(1-2 人周) |
| 前置依赖 | 无 |
| 关联 PRD | `agent-harness-prd.md` §8 T4(审批策略) |
| 关联分析 | `claude-code-analysis/analysis/04b-tool-call-implementation.md` §6(`runToolUse` 主干) |

---

## 1. 现状证据

这是 legion 当前**默认配置下就存在的失控风险**,共三处证据:

1. **`Approval::Prompt` 实质等同 `Required`**:`legion-tools/src/policy.rs:11-12` 注释自认 *"for the MVP"*;`policy.rs:80-86` 的 `check_policy` 中 `Prompt` 与 `Required` **走同一分支**——即 `Prompt` 级别从未实现"询问用户",而是直接当作"需要审批但无询问通道"。
2. **决策器是同步纯函数,无法挂起**:`legion-runtime/src/tools.rs:128` 的 `CanUseToolFn` 是同步闭包,**无法 await 人工确认**。即使想实现"询问用户后放行",当前类型签名做不到。
3. **主循环根本没挂决策器**:`legion-runtime/src/agent_loop.rs:201` 调用工具时传入的 `can_use_tool` 是 **`None`**——即工具能否执行**完全依赖工具内部 Policy**,主循环不做任何拦截。

**结论**:legion 当前**没有真正的人工确认回路**。`exec`(默认 `Required`)在无 `allowFrom` 命中时会被 Policy 拒绝,但**任何被标为 `Off` 的工具(read/write/edit/web_fetch 等)可不经任何确认执行**;且 `Prompt` 级别是死代码。对于多通道 gateway(消息驱动、可能无人值守),这是安全隐患。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:审批策略可通过 `ApprovalGate` trait 自定义(默认实现 + 可插件)。
- **P2 安全作为不变量**:**默认配置下,危险工具(exec/write/delete-class)必须经人工确认或被显式 allow 放行**;`Prompt` 真正询问,不再等同 `Required`。
- **P3 增量**:无 ApprovalGate 配置时,行为退化为当前"按 Policy 决策"(向后兼容)。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:每次 allow/deny/prompt 产生 `tracing` 事件 + 审计记录。
- **P6 失败显式**:审批超时、用户拒绝、hook 阻断分类处理。
- **P7 测试**:allow/deny/prompt-timeout/hook-block/hook-modify 各有测试。

---

## 3. 架构设计

### 3.1 借鉴 Claude Code `runToolUse` 主干(04b §6)

Claude Code 的工具执行主干是一条清晰的管线:

```
schema 校验(Zod) → validateInput(语义校验) → backfill 隐式依赖
   → PreToolUse Hooks(可改 input / 阻断)
   → checkPermissions(allow / deny / ask)
   → tool.call()
   → 标准化 tool_result
```

legion 当前只有 schema 校验 + Policy,缺 **PreToolUse Hooks** 与 **真正的 ask(Prompt)回路**。本 gap 补齐这两段。

### 3.2 三态决策

```
ApprovalDecision
   ├── Allow                    // 直接放行
   ├── Deny { reason, permanent }// 拒绝(可记入会话级 deny 避免重复问)
   └── Prompt { message, risk }  // 需人工确认 → 进 ApprovalQueue
```

`Prompt` 不再等同 `Required`:它触发**异步询问用户**;`Required` 在无 approval gate 时**直接拒绝**(安全默认)。

### 3.3 审批回流通道

```
agent_loop 调用工具
   ▼
ApprovalGate.evaluate(req) → Prompt
   ▼
ApprovalQueue.enqueue(prompt_id) → 通过 channel provider 发审批消息给用户
   ▼  (WebChat: 卡片;Telegram: 回复键盘 "允许/拒绝")
用户回复 → channel provider → ApprovalQueue.resolve(prompt_id, decision)
   ▼
agent_loop 继续(放行或回填拒绝原因给 LLM)
```

无人值守通道(如 cron 触发)无用户接收审批 → `Prompt` 自动降级为 `Deny`(安全默认)。

### 3.4 Hooks 注入点

借鉴 Claude Code PreToolUse/PostToolUse hooks(可改 input、可阻断)。legion 用 trait 对象实现,接入 automation 的 hook 体系。

---

## 4. 接口设计(Rust)

### 4.1 异步决策器(`legion-tools`)

```rust
use async_trait::async_trait;
use std::sync::Arc;
use std::future::Future;
use std::pin::Pin;

pub enum ApprovalDecision {
    Allow,
    Deny { reason: DenyReason, permanent: bool },
    Prompt { message: String, risk: RiskLevel },
}

#[derive(Debug, Clone, Copy)]
pub enum RiskLevel { Low, Medium, High }
#[derive(Debug, Clone)]
pub enum DenyReason { Policy, UserRejected, Timeout, HookBlocked(String) }

pub struct ToolCallRequest {
    pub tool: String,
    pub input: serde_json::Value,
    pub agent_id: String,
    pub session_key: SessionKey,
    pub policy: Policy,
    pub workspace: std::path::PathBuf,
    pub interactive: bool,   // 该会话能否接收用户审批(cron/无人值守=false)
}

/// 决策门:评估 + (若 Prompt)等待人工。
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn evaluate(&self, req: &ToolCallRequest) -> ApprovalDecision;
    /// Prompt 后等待用户决定;返回最终 allow/deny。
    async fn await_decision(&self, prompt_id: &str, timeout: std::time::Duration)
        -> Result<bool, ApprovalError>;
}

// CanUseToolFn 从同步闭包改为异步(破坏性,见 §5)
pub type CanUseToolFn = Arc<
    dyn Fn(ToolCallRequest) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send>>
       + Send + Sync,
>;
```

### 4.2 默认 ApprovalGate 实现

```rust
pub struct DefaultApprovalGate {
    queue: Arc<ApprovalQueue>,
    session_denies: Arc<RwLock<HashSet<String>>>,  // 会话级永久 deny
}

#[async_trait]
impl ApprovalGate for DefaultApprovalGate {
    async fn evaluate(&self, req: &ToolCallRequest) -> ApprovalDecision {
        // 1. 会话级 deny 命中 → Deny
        // 2. Policy.allow_from 命中 → Allow
        // 3. Policy.approval == Off → Allow
        // 4. Policy.approval == Prompt:
        //      if req.interactive { Prompt } else { Deny(无人值守) }
        // 5. Policy.approval == Required:
        //      if req.interactive { Prompt } else { Deny }   // 真实询问,不再直接拒
    }
    async fn await_decision(&self, prompt_id: &str, timeout) -> Result<bool> {
        self.queue.wait(prompt_id, timeout).await
    }
}
```

### 4.3 审批队列

```rust
pub struct ApprovalQueue {
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    notifier: Arc<dyn ApprovalNotifier>,   // 发消息给 channel
}
impl ApprovalQueue {
    pub async fn enqueue(&self, prompt_id: String, req: &ToolCallRequest)
        -> Result<oneshot::Receiver<bool>>;
    pub async fn resolve(&self, prompt_id: &str, allow: bool) -> Result<()>;
    pub async fn wait(&self, prompt_id: &str, timeout: Duration) -> Result<bool>;
}

#[async_trait]
pub trait ApprovalNotifier: Send + Sync {
    /// 通过 originating channel 发审批请求给用户。
    async fn request_approval(&self, req: &ToolCallRequest, prompt_id: &str) -> Result<()>;
}
```

### 4.4 Tool Hooks

```rust
#[async_trait]
pub trait ToolHook: Send + Sync {
    async fn pre_tool(&self, ctx: &ToolCallContext) -> HookOutcome;
    #[async_trait::async_trait(unused?)]  // 默认空实现
    async fn post_tool(&self, ctx: &ToolCallContext, result: &ToolResult) {}
}

pub enum HookOutcome {
    Continue,
    Block(String),          // 阻断,reason 回填 LLM
    ModifyInput(serde_json::Value),  // 改写工具输入(借鉴 PreToolUse)
}
```

---

## 5. 集成点

| 位置 | 改动 | 破坏性 |
|---|---|---|
| `legion-tools/src/policy.rs:11,80-86` | `Approval::Prompt` 与 `Required` 分流;`check_policy` 返回 `ApprovalDecision` 而非 bool。 | 是(签名变更) |
| `legion-runtime/src/tools.rs:128` | `CanUseToolFn` 同步 → 异步(`Pin<Box<Future>>`)。 | 是 |
| `legion-runtime/src/agent_loop.rs:201` | 传入真实 `ApprovalGate`(不再 `None`);工具执行前 `evaluate` + 必要时 `await_decision`。 | 否(补挂载) |
| `legion-runtime/src/tool_pipeline.rs:31-67` | 工具执行管线插入 hooks(`pre_tool` 在 approval 前,`post_tool` 在 call 后)。 | 否 |
| `legion-channel` | `ChannelProvider` 可选实现 `ApprovalNotifier`;WebChat/Telegram 发审批消息 + 接收回复回流 queue。 | 否 |
| `legion-core/src/config.rs` | 新增 `approval: ApprovalConfig { timeoutMs, defaultMode, sessionDeny }`。 | 否 |

---

## 6. 风险与权衡

### 6.1 `Prompt` vs `Required` 语义重定义(破坏性)
**当前**:`Prompt≡Required`(都"需审批但无通道")。
**新设计**:
- `Prompt`:交互式会话 → 询问用户;无人值守 → Deny。
- `Required`:同上(二者合并为"需人工确认")。或保留 `Required` 表示"即使交互也倾向拒绝,需 escalate"。
**取舍**:为减少语义混淆,Phase A 将 `Prompt` 与 `Required` 合并为"需确认",通过 `interactive` 字段区分行为;`Off` 仍直接放行。

### 6.2 无人值守降级(安全核心)
cron/heartbeat/后台任务触发的 agent turn,`interactive=false`。此时 `Prompt/Required` 工具**自动 Deny**——避免后台 agent 在无人时执行危险操作。这是借鉴之外、legion 特化(因为 Claude Code 是交互式 CLI,无此场景)。

### 6.3 审批超时
用户未回复 → `await_decision` 超时 → 视为 `Deny(Timeout)`。超时时长可配(默认 300s)。

### 6.4 因地制宜:异步审批的并发
Claude Code 用 TS Promise;legion 用 `oneshot::channel` + `Mutex<HashMap>`,审批状态跨 channel 回调与 agent_loop 两个异步任务同步。

### 6.5 会话级 deny(避免反复问)
用户对某工具在某会话拒绝一次 → 记入 `session_denies`,该会话内不再重复询问(借鉴 Claude Code compound command 检查)。可配 `permanent` 持久化。

### 6.6 Hooks 安全
`pre_tool` hook 可 `ModifyInput`,等同于改写工具参数。**约束**:hook 来源限定为系统/配置信任的来源(automation hook 目录),不来自用户消息;hook 代码在 Gateway 进程内执行,需文档警示。

---

## 7. 实现路线图

### 阶段 A(Phase A,~1 人周):异步决策器 + Prompt 回路
1. `ApprovalDecision`/`ApprovalGate`/`ApprovalQueue` 类型;`CanUseToolFn` 改异步。
2. `policy.rs` 分流 `Prompt`/`Required`;`DefaultApprovalGate` 实现。
3. `agent_loop.rs:201` 挂载真实 gate。
4. `interactive` 字段;无人值守 Deny 降级。
5. **验收**:`Prompt` 工具触发询问(测试用 mock notifier);无人值守时 Deny。

### 阶段 B(Phase A,~0.5 人周):channel 审批回流
1. `ApprovalNotifier` trait;WebChat 实现(卡片 + 回复回流)。
2. Telegram 实现(回复键盘)。
3. 审批超时。
4. **验收**:WebChat/Telegram 用户能收到审批请求并回复,gate 据此放行/拒绝。

### 阶段 C(Phase A 尾,~0.5 人周):Hooks + 会话级 deny
1. `ToolHook` trait;tool_pipeline 插入 pre/post hook。
2. 会话级 deny 记录。
3. 审计记录(每次决策落 `tracing` + 可选 audit log)。
4. **验收**:hook 能阻断/改写工具输入;会话级 deny 不重复询问。

---

## 8. 验收标准

- [x] `Approval::Prompt` 不再等同 `Required`(行为分流测试)。
- [x] 主循环挂载真实 `ApprovalGate`,不再传 `None`(`agent_loop.rs` 审查)。
- [x] `Prompt`/`Required` 工具在交互式会话触发用户询问(mock notifier 测试)。
- [x] 无人值守会话(`interactive=false`)的 `Prompt`/`Required` 工具自动 Deny(安全测试)。
- [x] WebChat/Telegram 用户可收到审批请求并回复,agent 据此放行/拒绝(机制已实现,端到端 E2E 待 channel 供应商测试环境)。
- [x] 审批超时(默认 300s)→ Deny。
- [ ] `pre_tool` hook 能阻断工具(返回 Block)与改写输入(返回 ModifyInput)。
- [x] 会话级 deny 不重复询问同工具。
- [ ] 每次 allow/deny/prompt 有结构化审计记录(当前已有 `tracing` 事件,审计日志待 Phase C)。
- [x] 无 ApprovalGate 配置时退化为当前 Policy 决策(回归)。
- [x] `AGENTS.md` 更新审批章节,声明 `Prompt` 真实询问语义。

---

*下一篇:[`sandbox-isolation.md`](./sandbox-isolation.md) · 返回类目:[`_index.md`](./_index.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
