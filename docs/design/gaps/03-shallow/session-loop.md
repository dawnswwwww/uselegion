# Gap:Session Loop 作用域缺失(local 模式 `/loop` 强制依赖 gateway)

> **实施状态**:Phase A 已实施(2026-07-16)。`LocalDriver` 内嵌 `CronScheduler`,TUI local 模式下 `/loop` 可用。Phase B/C(UX 命令、`--global` 标志、配置开关)待后续。

| 字段 | 值 |
|---|---|
| 类目 | [03-shallow](./_index.md)(内核浅化) |
| 优先级 | P2(UX 深度) |
| 工作量 | S-M(3-7 人日) |
| 前置依赖 | automation-advanced(已落地 cron/webhook/flow) |
| 关联 PRD | `agent-harness-prd.md` §9 A3/A4 |
| 关联分析 | 用户交互反馈:local 模式 TUI 中 `/loop` 因“embedded mode has no cron scheduler”被拒绝 |

---

## 1. 现状证据

legion 的 cron 调度能力已在 `automation-advanced` Phase C 完整落地,但 `/loop` 命令的**作用域与生命周期设计是单层的**:

- **调度能力已通用化**:`crates/legion-automation/src/cron.rs:247-259` 的 `CronScheduler::new` 只依赖 `CronJobStore` trait、`TaskStore`、`Harness` 与 `Config`,不依赖 gateway 特有代码。
- **Store 已抽象化**:`cron.rs:92-100` 定义了 `CronJobStore` trait,`JsonlCronJobStore`(`cron.rs:103-119`) 只是其中一种持久化实现。
- **gateway 独占持久调度**:`crates/legion-cli/src/main.rs:439-441` 中 gateway 用 `JsonlCronJobStore::open(~/.legion/automation/cron.jsonl)` 管理全局任务。
- **local 模式直接拒绝**:`crates/legion-cli/src/driver.rs:421-424` 的 `LocalDriver::schedule_loop` 直接返回错误:
  > `/loop requires the gateway (embedded mode has no cron scheduler). Start the gateway with `legion gateway start`.`
- **gateway 驱动转发**:`driver.rs:196-222` 的 `WsDriver::schedule_loop` 通过 `cron.add` RPC 把任务交给 gateway。
- **TUI 统一入口**:`crates/legion-cli/src/tui.rs:979` 的 `OutboundControl::ScheduleLoop` 对两种 driver 调用同一接口,没有区分作用域。

**结论**:技术上门控过重。`CronScheduler` 完全可以在 local 进程中实例化,只是当前没有为 CLI/TUI 提供一个**进程级**或**会话级**的 store 与生命周期策略。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:调度器本身保持通用;新增一种 store 实现即可支持新作用域,不改 `CronScheduler`。
- **P2 安全**:session loop 不得污染全局 cron store;任务 ID/路径隔离,避免多进程冲突。
- **P3 增量**:默认行为不变(`legion loop` CLI 子命令仍走 gateway);local TUI `/loop` 新增 session-scope 能力。
- **P4 证据**:现状见 §1;借鉴无外部源码,需求来自实际使用反馈。
- **P5 可观测**:session loop 的调度、执行、失败均应进入 unified log / session metrics(已落地 telemetry)。
- **P6 失败显式**:session loop 在 TUI 退出时主动清理或明确标注“会话结束,任务已丢弃”。
- **P7 测试**:LocalDriver schedule/list/remove/run/tick 全路径单元测试 + TUI `/loop` 集成测试。

---

## 3. 架构设计

### 3.1 作用域分层

```
┌─────────────────────────────────────────────────────────────┐
│                         Global Scope                         │
│  gateway process                                             │
│  store: ~/.legion/automation/cron.jsonl                      │
│  lifecycle: gateway 启动 → 调度; gateway 停止 → 暂停         │
│  entry:  legion loop ...                                     │
│          TUI --gateway 模式下 /loop                          │
└─────────────────────────────────────────────────────────────┘
                              ▲
                              │ 能力复用: 同一 CronScheduler + CronJobStore trait
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                        Session Scope                         │
│  TUI local process                                           │
│  store: ~/.legion/agents/<agent>/sessions/<peer>/cron.jsonl  │
│  lifecycle: TUI 启动 → 加载; TUI 退出 → 任务失效(可保留文件) │
│  entry:  TUI local 模式下 /loop                              │
└─────────────────────────────────────────────────────────────┘
```

### 3.2 关键设计决策

1. **能力复用,生命周期分离**
   - `CronScheduler`、`AddJobRequest`、`CronJob`、`CronJobStore` trait 全部复用 `legion-automation`。
   - 不同作用域仅替换 `CronJobStore` 实现与 cron-loop task 的宿主进程。

2. **Session store 默认持久但绑定进程**
   - 使用 `JsonlCronJobStore` 写到 session 目录,允许 TUI 崩溃重启后恢复本次会话的 loop。
   - 但**不保证跨 TUI 实例共享**:新 TUI 启动时默认创建新 session/peer,旧 session 的 loop 不自动激活。

3. **任务 ID 加作用域前缀**
   - Global:`cron-{ts}-{n}`(保持现有)。
   - Session:`session-cron-{ts}-{n}`。
   - 避免 `legion cron list` 误把 session job 当全局任务。

4. **失败与清理显式**
   - TUI 正常退出时,可选择:
     - (A) 保留 session cron 文件,作为“可恢复”记录;
     - (B) 删除未完成的 session loop。
   - 推荐默认 (A),并在退出日志中写入 `SessionLoopTerminated` 事件。

---

## 4. 接口设计

### 4.1 `CronJobStore` 新实现(可选)

当前已有 `JsonlCronJobStore`,可直接复用。若未来需要内存级 session store,可新增:

```rust
pub struct InMemoryCronJobStore {
    jobs: Mutex<HashMap<String, CronJob>>,
}

#[async_trait]
impl CronJobStore for InMemoryCronJobStore {
    async fn create(&self, job: CronJob) -> Result<(), CronError> { ... }
    async fn update(&self, job: CronJob) -> Result<(), CronError> { ... }
    async fn remove(&self, id: &str) -> Result<(), CronError> { ... }
    async fn list(&self) -> Result<Vec<CronJob>, CronError> { ... }
    async fn get(&self, id: &str) -> Result<Option<CronJob>, CronError> { ... }
}
```

### 4.2 `LocalDriver` 扩展

```rust
pub struct LocalDriver {
    host: AgentHost,
    session_key: String,
    config: Config,
    // 新增
    cron_scheduler: Option<Arc<CronScheduler>>,
}

impl LocalDriver {
    pub async fn new(host: AgentHost, session_key: String, config: Config) -> Result<Self, CliError> {
        let scheduler = if config.automation.cron.enabled_for_local {
            let session_dir = session_path(&session_key)?;
            let job_store = JsonlCronJobStore::open(session_dir.join("cron.jsonl")).await?;
            let task_store = JsonlTaskStore::open(session_dir.join("tasks.jsonl")).await?;
            let scheduler = Arc::new(CronScheduler::new(
                Arc::new(job_store),
                Arc::new(task_store),
                Arc::new(host.harness()), // 或 host 暴露的 runtime
                config.clone(),
            ));
            tokio::spawn(cron_loop(scheduler.clone()));
            Some(scheduler)
        } else {
            None
        };
        Ok(Self { host, session_key, config, cron_scheduler: scheduler })
    }
}

#[async_trait]
impl TurnDriver for LocalDriver {
    async fn schedule_loop(&self, cron: &str, prompt: &str) -> Result<String, CliError> {
        let scheduler = self.cron_scheduler.as_ref()
            .ok_or_else(|| CliError::Other("local cron scheduler is disabled".into()))?;
        let job = scheduler
            .add(AddJobRequest {
                schedule: cron.to_string(),
                agent_id: session_agent_id(&self.session_key).unwrap_or("main").to_string(),
                message: prompt.to_string(),
                ..Default::default()
            })
            .await?;
        Ok(job.id)
    }

    // resolve_approval / resolve_question / run_turn 保持不变
}
```

### 4.3 `TurnDriver` trait 新增 list/remove(可选)

为了支持 TUI 内查看/删除 session loop,可在 trait 上扩展:

```rust
#[async_trait]
pub trait TurnDriver: Send + Sync {
    // ... existing methods ...

    /// List loops visible in the current scope.
    async fn list_loops(&self) -> Result<Vec<LoopSummary>, CliError> {
        Ok(vec![]) // default no-op for drivers that don't support it
    }

    /// Remove a loop by id. Must check scope (global vs session) before acting.
    async fn remove_loop(&self, id: &str) -> Result<(), CliError> {
        Err(CliError::Other("remove_loop not supported".into()))
    }
}
```

### 4.4 UX 命令设计

TUI 内:

```text
/loop every 5m check email          # session scope (local mode)
/loop --global every 5m check email  # global scope (local mode 下显式走 gateway)
/loops                               # 列出当前 scope 的 loop
/loop --stop <id>                    # 删除当前 scope 的 loop
```

CLI 子命令保持现有行为,但增加作用域提示:

```bash
legion loop "check email" every 5m
# → "Scheduled as global cron job <id> (managed by gateway)."
```

---

## 5. 集成点

| 模块 | 集成内容 |
|---|---|
| `crates/legion-automation/src/cron.rs` | 新增 `InMemoryCronJobStore`(可选);确认 `CronScheduler` 不依赖 gateway 特有类型。 |
| `crates/legion-cli/src/driver.rs` | `LocalDriver` 内嵌 `CronScheduler`,使用 session 私有 store;实现 `schedule_loop` 并可选扩展 `list_loops`/`remove_loop`。 |
| `crates/legion-cli/src/tui.rs` | `/loop` 在 local 模式下调用 `LocalDriver::schedule_loop`;错误提示改为区分“无 scheduler”与“gateway 未连接”。 |
| `crates/legion-cli/src/slash_commands.rs` | 解析 `/loop` 的 `--global` 等可选标志,并传递给 driver。 |
| `crates/legion-cli/src/main.rs` | `legion loop` 子命令保持走 gateway,输出明确提示“global”。 |
| `crates/legion-core/src/config.rs` | 新增 `automation.cron.enabled_for_local` 开关,默认 `true`。 |
| `crates/legion-telemetry/src/client.rs` | session loop 的调度/执行事件复用现有 `SessionMetric::ToolCalled`/`TurnCompleted` 等,或新增 `SessionLoopFired`。 |

---

## 6. 风险与权衡

| 风险 | 影响 | 缓解 |
|---|---|---|
| **多 TUI 实例各自有独立 session loop** | 用户可能困惑“我在另一个窗口设的 loop 怎么没了” | UX 上明确标注 scope;`legion cron list` 仅显示 global;TUI 内 `/loops` 显示 session scope |
| **session loop 文件残留** | `~/.legion/agents/*/sessions/*/cron.jsonl` 可能积累 | session 启动时清理旧 session 的未完成任务;或设 TTL(参照 `sessions.ttlDays`) |
| **LocalDriver 持有额外 runtime 资源** | 每个 local TUI 多一个 cron tick task 和一个 store | 默认开启但可配置关闭;`CronScheduler::tick` 本身开销低(仅 list + 检查 due time) |
| **任务执行上下文与 gateway 不一致** | session loop 用 local `AgentHost`,可能缺少 gateway 的某些状态(如已配对设备、channel 连接) | session loop 仅用于“本地 agent turn”,不用于 channel 触发,这是符合预期的 |
| **一次性 CLI 无法承载 loop** | `legion agent "..."` 秒退,不能托管长期 loop | 保持 `legion agent` 不支持 `/loop`;TUI local 模式才支持 |

---

## 7. 实现路线图

### Phase A — 最小可用(MVP,3-4 人日)

1. `LocalDriver` 内嵌 `CronScheduler`,使用 session 目录下的 `JsonlCronJobStore`。
2. `LocalDriver::schedule_loop` 从“报错”改为“实际创建 session job”。
3. TUI local 模式下 `/loop` 可用,任务 ID 加 `session-cron-` 前缀。
4. TUI 退出时保留 session cron 文件(便于崩溃恢复)。
5. 单元测试覆盖 local schedule/run/remove。

### Phase B — UX 与可观测性(2-3 人日)

1. 新增 `/loops` 命令查看当前 session loop。
2. 新增 `/loop --stop <id>` 删除 session loop。
3. 错误提示优化:local 模式下 gateway 不可达时,`/loop` 不再提示“start gateway”。
4. session loop 事件进入 telemetry(`SessionLoopFired` / `SessionLoopCompleted`)。
5. session 启动时清理过期(如 >7 天)的 session cron 文件。

### Phase C — 可选增强(未来)

1. `legion loop --session` 子命令:从 CLI 启动一个 session loop 并 attach 到指定 session。
2. `--global` 显式转发到 gateway,即使 TUI 在 local 模式。
3. 配置 `automation.cron.enabled_for_local` 默认 `true`,允许用户显式关闭。

---

## 8. 验收标准

### 8.1 功能验收

- [ ] TUI local 模式下输入 `/loop every 1m say hi` 成功创建任务,返回 `session-cron-` 前缀 ID。
- [ ] 任务在 TUI 存活期间每分钟触发一次 agent run,输出“hi”。
- [ ] TUI 退出后重启新的 TUI,旧 session loop 不自动激活(新 session/新 peer)。
- [ ] TUI `--gateway` 模式下 `/loop` 仍走 gateway,返回 `cron-` 前缀 ID。
- [ ] `legion loop` CLI 子命令仍走 gateway,行为不变。

### 8.2 自动化测试

- [ ] `cargo test -p legion-cli driver::tests::local_driver_schedules_session_loop`
- [ ] `cargo test -p legion-cli driver::tests::local_driver_runs_session_loop_tick`
- [ ] `cargo test -p legion-cli tui::tests::slash_loop_in_local_mode_creates_session_job`
- [ ] `cargo test --workspace --all-targets` 全绿。

### 8.3 文档与日志

- [ ] `docs/DEVLOG.md` 新增条目,记录 session-loop Phase A/B 落地。
- [ ] 本文件顶部“实施状态”更新为已完成,并指向 DEVLOG 日期。
- [ ] `AGENTS.md` 若涉及 CLI 行为约定,同步更新。

---

*返回类目索引:[`_index.md`](./_index.md)*  
*返回总览:[`../00-overview.md`](../00-overview.md)*
