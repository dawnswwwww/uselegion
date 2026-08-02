# Gap:Automation 高级特性(Standing Orders/Commitments/Task Flow DAG)

| 字段 | 值 |
|---|---|
| 类目 | [04-breadth](./_index.md)(生态广度) |
| 优先级 | P2 |
| 工作量 | M |
| 前置依赖 | [prompt-management](../03-shallow/prompt-management.md)(Standing Orders 注入);[memory-layers](../03-shallow/memory-layers.md)(Commitments 复用轻量 LLM) |
| 关联 PRD | `agent-harness-prd.md` §9 A4/A5/A7(Standing Orders/Commitments/Task Flow) |
| 关联参考 | `docs/openclaw_raw/automation/standing-orders.md`、`automation/clawflow.md`、`automation/taskflow.md`、`automation/webhook.md` |
| 状态 | ✅ 已实施(Phase A+B+C,2026-07-11,见 DEVLOG;Phase D 条件分支/revision tracking 暂不承诺) |

---

## 1. 现状证据

legion 的 automation **基础扎实(亮点)**,缺高级编排:

- **Cron 真实**:`legion-automation/src/cron.rs`(675 行),JSONL 持久化 `JsonlCronJobStore`,5 字段 cron + 一次性 `__at__`,`tick` 循环,完整测试。
- **Heartbeat 真实**:`heartbeat.rs`(173 行),周期读 HEARTBEAT.md,明确不建 task/不刷 idle。
- **Hooks 已废弃**:原 `hooks.rs`(进程级 6 事件脚本钩子,大半死代码)已删除,事件出口能力收编进独立的 `/events` 版本化事件总线(见 [`docs/design/events-bus.md`](../events-bus.md))——外部工具/GUI 通过 `AttachSession` 订阅 session 生命周期/工具/文本流。
- **Task runner 真实**:`task_runner.rs`(424 行)有 `depends_on` 依赖解析 + 状态机 + 超时。
- **缺 Standing Orders(A4)**:无"每次会话注入的持久授权/边界"机制。
- **缺 Inferred Commitments(A5)**:无"自然语言推断的短期跟进"——对话中说"明天提醒我"不会自动生成 task。
- **Task Flow 不是真 DAG(A7)**:`task_runner` 的 `depends_on` 仅按顺序执行,**无多步骤编排引擎、无条件分支、无 revision tracking**。
- **Cron 仅表达式 + --at**:无 webhook/PubSub 触发源。

**结论**:automation 的"定时 + 钩子 + 任务"基座是 legion 最成熟的子系统之一;缺的是"智能编排层"。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:新自动化类型 = 实现 task kind / trigger,复用现有 task_runner。
- **P2 安全**:Standing Orders 是**授权注入**,来源受控(仅配置/skill,非用户消息);Commitments 生成受 cooldown 限。
- **P3 增量**:新特性默认关;现有 cron/heartbeat/task 行为不变。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:Standing Order 注入、Commitment 生成、Flow 执行产生 `tracing`。
- **P6 失败显式**:Commitment LLM 失败静默;Flow 步骤失败按策略(abort/continue/branch)。
- **P7 测试**:Standing Order 注入、Commitment 生成、DAG 拓扑执行各有测试。

---

## 3. 架构设计

### 3.1 Standing Orders(借鉴 OpenClaw `standing-orders.md`)

每次会话 turn 注入的**持久授权/边界声明**。本质是 prompt section(配合 [prompt-management](../03-shallow/prompt-management.md))。

```
config/AGENTS.md 定义 Standing Orders
   ▼
每次 assemble_system_prompt 注入为 section
   ▼
agent 持续遵守(如"只读 prod 数据"、"必须先写测试")
```

### 3.2 Inferred Commitments(借鉴 OpenClaw `concepts/commitments.md`)

对话中自然语言提及的跟进("明天提醒我复查 X")→ 后台轻量 LLM 推断 → 生成短期 task(走 task_runner/cron)。

```
turn 结束(后台)
   ▼
CommitmentExtractor.scan(recent_msgs)
   ▼ 轻量 LLM 判断是否有承诺
生成 Commitment { description, due } → 写 task_store 或 cron(one-shot)
   ▼
cooldown 防频繁
```

### 3.3 Task Flow DAG(借鉴 OpenClaw `automation/taskflow.md`)

复用现有 `task_runner` 的 `depends_on`,升级为多步骤编排:

```
TaskFlow { steps: [FlowStep], revisions: [...] }
FlowStep { name, task, depends_on, condition }
   ▼
执行:拓扑排序 + 并行同层 + 条件分支
   ▼ 失败
策略:abort(默认)/ continue / branch
   ▼
revision tracking(每次执行留快照,可回溯)
```

### 3.4 Cron webhook 触发源

```rust
pub enum CronTrigger {
    Schedule(CronExpr),
    At(DateTime),
    Webhook { secret: String, event: String },   // 新增
}
```
Gateway 暴露 `/webhook/<id>` 端点,校验 secret 后触发对应 cron job。

---

## 4. 接口设计(Rust)

### 4.1 Standing Orders(`legion-automation` + prompt 注入)

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StandingOrder {
    pub id: String,
    pub instruction: String,
    pub scope: StandingScope,
    #[serde(default = "default_true")]
    pub enabled: bool,
}
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StandingScope { Global, Agent(String) }

// 作为 PromptSection 注入(配合 prompt-management)
impl StandingOrder {
    pub fn to_section(&self) -> PromptSection {
        PromptSection { id: SectionId::Other("standing_orders".into()),
                        content: self.instruction.clone(),
                        source: SectionSource::Default, cacheable: true,
                        max_tokens: Some(500) }
    }
}
```

### 4.2 Inferred Commitments

```rust
pub struct CommitmentExtractor { router: Arc<dyn ProviderRouter> }

#[derive(Debug, Clone)]
pub struct Commitment {
    pub description: String,
    pub due: Option<String>,          // ISO 时间(由 args 传入,见 §6)
    pub source_session: SessionKey,
    pub status: CommitmentStatus,
}
#[derive(Debug, Clone, Copy)]
pub enum CommitmentStatus { Pending, Fulfilled, Expired }

impl CommitmentExtractor {
    pub async fn extract(&self, msgs: &[Message]) -> Result<Vec<Commitment>> {
        // 轻量 LLM + 限制 prompt;cooldown;去重
    }
}
```

### 4.3 Task Flow DAG(复用 task_runner)

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskFlow {
    pub id: String,
    pub steps: Vec<FlowStep>,
    pub on_failure: FlowFailurePolicy,
    pub revisions: Vec<FlowRevision>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowStep {
    pub name: String,
    pub task: TaskSpec,               // 复用现有 Task
    pub depends_on: Vec<String>,
    pub condition: Option<Condition>, // 条件分支
}

#[derive(Debug, Clone, Copy, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FlowFailurePolicy { #[default] Abort, Continue, Branch }

// 执行:复用 task_runner.rs:108-134 的依赖解析 + 拓扑排序
```

### 4.4 Cron webhook

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CronTrigger {
    Schedule { expr: String },
    At { when: String },
    Webhook { secret: String, event: String },
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-automation/src/cron.rs` | `CronTrigger` 枚举;webhook 端点触发。 |
| `legion-automation/src/tasks.rs` + `task_runner.rs` | `TaskFlow`/`FlowStep`/条件分支;复用 `depends_on` 拓扑;revision tracking。 |
| 新增 `legion-automation/src/standing_orders.rs` | `StandingOrder` 类型 + 持久化。 |
| 新增 `legion-automation/src/commitments.rs` | `CommitmentExtractor`(复用 router 轻量 LLM)。 |
| `legion-runtime/src/context.rs` | assemble_system_prompt 注入 `standing_orders` section。 |
| `legion-gateway` | `/webhook/<id>` 端点(secret 校验)。 |
| `legion-core/src/config.rs` | `standingOrders`/`commitments`/`flows` 配置。 |
| `legion-cli` | `legion flows list/run`、`legion commitments list`。 |

---

## 6. 风险与权衡

### 6.1 Standing Orders 的安全语义
Standing Orders 是**授权注入**(如"允许直接执行 exec")。**约束**:来源仅限配置/AGENTS.md/skill,**绝不来自用户消息**(防 prompt injection 提权);注入位置在 system prompt(高优先级),文档警示其等价于提升 agent 权限。

### 6.2 Inferred Commitments 的噪声
LLM 推断可能产生假承诺(把闲聊误判为承诺)。**缓解**:cooldown(同 session 短期不重复推断);confidence 阈值;用户可关闭(`commitments.enabled: false`);生成的 task 标注来源 `inferred`,可审阅。

### 6.3 Task Flow DAG 的复杂度
真正 DAG(条件分支 + 并行 + 回溯)复杂。**取舍**:Phase C 先做线性 + 依赖并行(复用现有 `depends_on`),条件分支/revision tracking 列后续;避免一上来做全功能编排引擎。

### 6.4 webhook 触发的安全
`/webhook/<id>` 暴露在 Gateway。**缓解**:secret 校验(HMAC);默认绑定 loopback;非 loopback 需 auth mode ≠ none(复用现有 gateway auth 约束)。

### 6.5 因地制宜:时间处理
Commitment 的 `due` 是时间,但 legion 工具环境可能无稳定时钟(参考工作流约束)。**缓解**:`due` 由调用方/args 传入绝对时间;`CommitmentExtractor` 只推断"相对描述",由 cron 转绝对(借鉴 OpenClaw timezone 处理)。

### 6.6 与 multi-agent 的协同
Task Flow 的步骤可以是 `subagent` task(配合 [multi-agent](../02-missing/multi-agent.md) 的 Coordinator 模式)。两者共享 task 抽象,Flow 是多 agent 编排的上层。

---

## 7. 实现路线图

### 阶段 A(Phase C,~0.5 人周):Standing Orders — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `StandingOrder { id, instruction, enabled }` 定义于 `legion-core/src/config.rs`;`AgentDefaults.standingOrders`(全局 scope)+ `AgentConfig.standingOrders`(per-agent scope)——**设计简化**:scope 由声明位置表达,未做 §4.1 的 `StandingScope` enum。✅
2. `SectionId::StandingOrders`;`assemble_system_prompt(_report)` 加 `standing_orders` 参数,enabled 非空时注入单个 cacheable section(`max_tokens` 2000,custom Base 之后、bootstrap 之前)+ `tracing::info!`;`agent_loop.rs` 合并全局在前传入。✅
3. **验收**:7 个新测试(config 解析 3 + 注入 4:enabled 注入/disabled 跳过/空 vec 无 section/全 disabled 无 section);来源仅限配置(类型只存在于 config,无任何从消息构造的路径);全量 26 suite 全绿,clippy/fmt 干净。

### 阶段 B(Phase C,~0.5 人周):Inferred Commitments — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `CommitmentExtractor` trait 定义于 `legion-runtime/src/commitments.rs`(fire-and-forget `spawn_extract`,照 `AgentMessenger` 模式:trait 在 runtime,实现在 automation——automation 依赖 runtime,反向成环);`AgentRuntime::with_commitment_extractor` 接线,`agent_loop` turn 结束处照 auto_extract 调用。✅
2. `LlmCommitmentExtractor`(`legion-automation/src/commitments.rs`,新建):轻量 LLM 从对话抽取 `{description, due(RFC3339 UTC)}`(prompt 注入当前 UTC 时间供相对时间推算,§6.5);SecretScanner 丢弃含密钥候选;过去 due/非法 JSON 跳过;生成 `schedule="__at__"` 的一次性 `CronJob`(id `commitment:{agent}:{hash}`,upsert 天然去重);per-agent cooldown;失败全部 warn 吞掉。✅
3. 顶层 `commitments` 配置(`CommitmentsConfig`,照 `autoExtract` 模式,enabled 默认 false);gateway 把 cron store 创建提前到 runtime 构建之前,commitment extractor 与 cron scheduler 共享同一实例(无双开);`legion commitments list` CLI(本地读 cron.jsonl,过滤 `commitment:` 前缀,按 due 排序)。✅
4. **验收**:8 个新测试(automation 6:正常生成/密钥丢弃/过去 due 跳过/冷却抑制/包裹文本/垃圾输入;config 2);`commitments.enabled: false` 时 extractor 不构建(回归);全量 26 suite 全绿,clippy/fmt 干净。**live LLM 未 E2E。**

### 阶段 C(Phase C,~0.5 人周):Task Flow DAG + Cron webhook — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `TaskFlow`/`FlowStep`/`FlowFailurePolicy`(abort 默认/continue)定义于 `legion-core/src/config.rs`,顶层 `flows: Vec<TaskFlow>` 声明式配置;`FlowRunner`(`legion-automation/src/flow.rs`,新建):预校验(重名/未知依赖)→ 按层 `FuturesUnordered` **并发**执行(同层 step 真正并行)→ abort 全跳过 / continue 仅跳传递依赖(`transitive_dependents` 纯函数)→ 无 ready 判循环;`FlowReport` 序列化经 `flows.run` WS RPC 返回;session id `agent:{agent}:flow:{flow}:{step}`。✅
2. `CronJob`/`AddJobRequest` 加 `webhook_secret`(serde 向后兼容旧 JSONL);webhook-only job 用 `schedule="__webhook__"`(有 secret 才放行,`compute_next_run` 返回 None 永不走时钟);`verify_webhook_signature`(HMAC-SHA256 + 手写 hex + 逐字节 XOR 常量时间比较,未加 subtle);`CronScheduler::get_job`;Gateway `POST /webhook/{id}`(无 job/无 secret → 404 不泄露存在性,签名缺失/无效 → 401,有效 → `scheduler.run` 返 task id)。✅
3. CLI:`legion flows list|run <id>`(RPC,照 Tasks 模式);`legion cron add` 加 `--webhook-secret`。✅
4. **验收**:19 个新测试(config 3 + flow 8:线性/菱形并发达 max≥2/abort/continue/循环/未知依赖/重名/传递依赖纯函数;cron 7:签名四形态/webhook-only add/无 secret 拒绝/get_job;gateway 集成 1 含 5 场景:404×2/401×2/**200 有效签名触发返 task id**——temp HOME + 预置 cron.jsonl 起真实 Gateway);非 loopback 安全由既有不变量覆盖(config 校验拒绝非 loopback + auth.mode none);全量 27 suite 全绿,clippy/fmt 干净。

### 阶段 D(P3):条件分支 + revision tracking
- Flow 条件分支、执行快照回溯。暂不承诺全功能。

---

## 8. 验收标准

- [x] 配置的 Standing Order 注入 system prompt(来源仅配置/AGENTS.md,非用户消息,安全测试)。(Phase A)
- [x] Inferred Commitments 从对话生成 task/cron,标注 `inferred`(id 前缀 `commitment:` + 一次性 `__at__` cron job);cooldown 生效(测试抑制第二次)。(Phase B)
- [x] `commitments.enabled: false` 时不推断(extractor 不构建,回归测试)。(Phase B)
- [x] Task Flow 多步骤按 `depends_on` 并行执行(同层 `FuturesUnordered` 并发,菱形测试并发峰值 ≥2);`on_failure: Abort` 正确中止(后续全 Skipped)。(Phase C)
- [x] Cron webhook 端点 HMAC secret 校验(401/404/200 集成测试);非 loopback 安全由既有 config 校验不变量覆盖(拒绝非 loopback + `auth.mode: none`)。(Phase C)
- [x] 现有 cron/heartbeat/hooks/task 行为不变(全量 27 suite 回归全绿;webhook_secret serde 向后兼容旧 JSONL)。
- [x] Standing Order 注入、Commitment 生成、Flow 执行有 `tracing`。
- [x] `legion flows list/run`、`legion commitments list` CLI 可用。
- [x] `AGENTS.md` 更新 automation 章节(Standing Orders/Commitments/Task Flow/webhook 声明)。

---

*上一篇:[`tools-p1p2.md`](./tools-p1p2.md) · 返回类目:[`_index.md`](./_index.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
