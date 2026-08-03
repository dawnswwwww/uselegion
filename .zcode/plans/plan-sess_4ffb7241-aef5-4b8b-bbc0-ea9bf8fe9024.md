# 实现计划：Legion 外部事件订阅总线 `/events`

## 目标与非目标

**目标**：为 Legion 加一条独立、版本化的出口事件总线，让外部工具/未来 GUI 订阅 session 生命周期事件。形态 = 学 jcode-harness-api 的 AttachSession 模型，但复用 Legion 已有的 `legion-protocol` 共享 crate 范式 + axum WS 基建 + NodeManager 注册表模式。

**非目标（明确不做，避免范围爬升）**：block/deny 语义、改写 tool input、订阅过滤(agent/tag)、全局跨 session 流、额外持久化 RunEvent、SSE、GUI（本次只做管道，GUI 是未来首个消费者，设计保证它能接上）、channel/cron 触发的 run 的事件订阅（v1 仅 WS `agent` 路径，留作 follow-up）。

**已锁定的设计决策**（来自 grilling）：
1. 纯出口订阅型（fan-out only，不碰 ApprovalNotifier/policy）
2. 独立 `/events` WS 端点 + 独立 semver schema（与内部 `/ws` 的 `WsFrame` 隔离）
3. 废弃 `HookRunner`（subtraction，收编进统一事件词汇表）
4. 向前流 + 历史 ChatMessage（`sessions.history` 既有路径，单一真相源，不新增存储）
5. attach 到单个 session（ListSessions + AttachSession/DetachSession）
6. ListSessions 仅返回活跃 session（注册表里有条目的）
7. schema = 稳定对外枚举 + 薄映射层（从已构建的 agent payload 映射，**不改 `drive_run_stream` 签名**）
8. 非 loopback 硬性拒绝 `/events`（对应 AGENTS.md 安全条款）
9. TokenDelta 在 v1 schema 内且实时 fan-out（GUI 后续做实时聊天 UI 的前提）

## 数据流

```
run_loop (emit RunEvent)
  → drive_run_stream (turn.rs:214) 把 RunEvent 经 run_event_to_payload 转成 payload
  → emit(WsFrame::event("agent", payload))   ← turn.rs:254/260
  → handle_agent 的闭包 (ws_rpc.rs:378):
       ├─ tx.send(frame)                      ← 现有：发给发起方 /ws 连接（不动）
       └─ HarnessEvent::from_agent_payload()  ← 新增：薄映射
            → event_bus.publish(session_key, ev)  ← 新增：fan-out 给 /events 订阅者
  → /events 连接的订阅者 rx → 写回 socket
```

**关键不变量**：`drive_run_stream` 签名零改动；内部 `/ws` 协议零改动；fan-out 完全在 gateway 侧闭包里，与现有 `tx.send` 并行。

## Slice 1 — `legion-protocol`：harness 类型 + 映射

新文件 `crates/legion-protocol/src/harness.rs`，在 `lib.rs` 加 `pub mod harness;`。

**版本常量**：`pub const HARNESS_API_VERSION: u32 = 1;`（独立于内部 `CURRENT_PROTOCOL_REVISION`）。

**对外稳定事件枚举**（薄、稳定；内部 RunEvent 改动不直接影响它）：
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessEvent {
    RunStarted { session_key: String, run_id: String },
    RunFinished { session_key: String, run_id: String },
    RunErrored { session_key: String, run_id: String, error: String },
    AssistantTextDelta { run_id: String, delta: String },
    ToolStarted { run_id: String, tool_call: ToolCallView },
    ToolFinished { run_id: String, tool_call: ToolCallView, result: ToolResultView, canonical_meta: Option<Value> },
    // v1 预留枚举位，不 emit（v1.1 再接）：
    ContextCompacted { run_id: String, summary: String, tokens_compacted: Option<usize> },
    TodoListUpdated { run_id: String, items: Value },
}
```
`ToolCallView { id, name, arguments }`、`ToolResultView { content, is_error }` —— 独立小结构，避免把内部 `legion_runtime::tools::ToolCall` 暴露成公开 API（隔离）。字段名 camelCase per AGENTS.md（serde rename）。

**请求帧**（客户端→服务端，`#[serde(tag = "type", rename_all = "snake_case")]`）：
```rust
pub enum HarnessRequest {
    Hello { v: u32 },
    ListSessions,
    AttachSession { session_key: String },
    DetachSession,
    Ping,
}
```

**服务端帧**（服务端→客户端）：
```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessServerFrame {
    HelloOk { v: u32 },
    Ok,
    Error { message: String },
    SessionList { sessions: Vec<HarnessSessionSummary> },
    Attached { session_key: String, run_id: Option<String>, history: Vec<ChatMessageView> },
    Detached,
    Event { event: HarnessEvent },
    Pong,
}
pub struct HarnessSessionSummary {
    session_key: String, agent_id: String, peer_id: String,
    run_id: Option<String>, status: SessionStatus,  // Live | Idle
}
```
`history` 复用既有 `ChatMessage`（`legion_provider::types::ChatMessage` 已 Serialize）——直接引用，不造 View，避免重复模型（single source of truth）。`ChatMessageView` = 直接用 `legion_provider::types::ChatMessage`（legion-protocol 是否已依赖 legion-provider？需确认；若无，history 用 `serde_json::Value` 透传，避免新增 crate 依赖）。

**映射函数**（从 `agent` payload JSON 重建，零 host 改动的核心）：
```rust
impl HarnessEvent {
    /// 从 drive_run_stream 已构建的 agent event payload 映射。
    /// payload 含: stream, phase, state, delta, tool_call, result, canonical_meta?, run_id
    pub fn from_agent_payload(session_key: &str, payload: &Value) -> Option<HarnessEvent>
}
```
依据 `turn.rs:146-204` 的 payload 形状：读 `stream` 字段分发（lifecycle/assistant/tool/compaction/todo_update），`run_id` 从 payload 取（`turn.rs:201` 已 stamp）。compaction/todo_update 在 v1 返回 `None`（不 emit）。单元测试覆盖每个 stream 分支。

**Slice 1 测试**（同文件 `#[cfg(test)]`）：serde 往返每个变体；`from_agent_payload` 对 6 种 payload 形状的正确映射（含 canonical_meta=None 分支）；版本不匹配 Hello 的处理（在 handler 侧，此处只测纯映射）。

## Slice 2 — `legion-gateway`：EventBus + fan-out 接线

新文件 `crates/legion-gateway/src/events.rs`。

**EventBus**（模板 = `nodes/manager.rs:22` 的 `Arc<Mutex<HashMap>>`）：
```rust
struct Entry {
    agent_id: String, peer_id: String,
    run_id: Option<String>,
    subscribers: Vec<(SubId, mpsc::UnboundedSender<HarnessServerFrame>)>,
}
#[derive(Clone, Default)]
pub struct EventBus { sessions: Arc<Mutex<HashMap<String, Entry>>> }

impl EventBus {
    pub async fn register_run(&self, session_key, agent_id, peer_id, run_id)  // 解析自 parse_session_key
    pub async fn end_run(&self, session_key)   // run_id=None；若无订阅者则 GC 删条目
    pub async fn list(&self) -> Vec<HarnessSessionSummary>
    pub async fn subscribe(&self, session_key) -> (SubId, Option<String>, mpsc::UnboundedReceiver<HarnessServerFrame>)
                                              // 返回 (订阅id, 当前run_id?, rx)；条目不存在则创建
    pub async fn unsubscribe(&self, session_key, sub_id)  // GC 同上
    pub async fn publish(&self, session_key, HarnessEvent)  // fan-out
}
```
`parse_session_key` 来自 `legion_plugin_sdk`（解析 session_key 的 7 段取 agent_id/peer_id）；解析失败则存空串（健壮性）。

**背压策略**（已定）：每个订阅者用 bounded 不必要——复用内部既有的 **unbounded** 模型保持一致性最简；但为防 OOM，v1 用 **bounded(512)**，`publish` 对所有事件 `try_send`，任何 `Full`/`Closed` → 逐出该订阅者（其 rx 端收到断开 → GUI 重连重新 attach 拿 fresh history，自愈）。慢消费者被断开而非拖垮 gateway。单测覆盖：1 发布者 + 1 不排空的订阅者 → publish 触发逐出。

**接线点 1 — `handle_agent`**（`ws_rpc.rs:367-386`）：
- spawn 前：`let bus = state.event_bus.clone();` 解析 agent_id/peer_id，`bus.register_run(session_key.clone(), ...).await;`
- 改 emit 闭包（`ws_rpc.rs:378`）：
  ```rust
  let bus = state.event_bus.clone();
  let key = session_key.clone();
  move |frame| {
      let _ = tx.send(frame.clone());                       // 原有：发起方 /ws
      if let WsFrame::Event { event_type, payload, .. } = &frame {
          if event_type == "agent" {
              if let Some(ev) = HarnessEvent::from_agent_payload(&key, payload) {
                  let k = key.clone();
                  tokio::spawn(bus.clone().publish(k, ev)); // 或同步 async fn via try_lock
              }
          }
      }
  }
  ```
  注意 publish 是 async（持有 Mutex）；闭包是同步 `FnMut`。两种做法：(a) publish 内部用 `std::sync::Mutex`（HashMap 操作极短，同步锁可接受，无 await 持锁）→ publish 可写成同步 `pub fn publish(&self, ...)`，闭包直接调；(b) 用 blocking_spawn。**选 (a)：EventBus 用 `std::sync::Mutex`**，所有方法同步，因为操作是非 async 的纯 HashMap 增删 + try_send（try_send 同步）。这样闭包直接 `bus.publish(key, ev)`，无 spawn。与 NodeManager 的 `Arc<Mutex<>>` 一致（manager 也是 std Mutex）。锁定 (a)。
- spawn 内 `drive_run_stream` 返回后（成功或失败）：`bus.end_run(key).await;`（end_run 也可同步 std Mutex，则无需 await）。统一 std Mutex，全同步。
  需 clone key/bus 进 spawn。

**接线点 2 — `GatewayState`**（`websocket.rs:25-49`）：加字段 `pub event_bus: EventBus;`。在 `GatewayState{...}` 构造处（gateway.rs 内 grep 结构字面量）初始化 `event_bus: EventBus::default()`。

**Slice 2 测试**（events.rs `#[cfg(test)]`）：register/list/end_run 生命周期；publish 多订阅者收到；bounded 慢订阅者逐出；parse_session_key 解析 agent/peer。

## Slice 3 — `/events` 端点 + handler + 路由 + auth + loopback

**路由注册**（`gateway.rs:414-423` 块内加一行）：`.route("/events", get(events_handler))`。

**auth 复用**：`authenticate()`（`websocket.rs:406`）由 `fn` 改 `pub(crate) fn`；`parse_frame`/`frame_to_message`/`close_with` 同理 pub(crate)（events handler 同 crate 复用）。首帧要求 `Connect`（`ConnectParams` 带 `AuthCreds`），与 `/ws` 完全一致的握手。

**loopback 硬拒绝**：抽出 `pub(crate) fn is_loopback_bind(bind_host: &str) -> bool`（从 `authenticate` 内 `websocket.rs:420` 的逻辑提取），`/events` handler 入口检查 `if !is_loopback_bind(&state.config.gateway.bind_host) { close_with(..., "events endpoint requires loopback bind") }`。单测直接测 `is_loopback_bind`（"127.0.0.1"/"localhost"/"::1"→true，其余→false）——避免测试里真的 bind 非 loopback。

**`events_handler`**（镜像 `websocket_handler` `websocket.rs:130`）：`WebSocketUpgrade` + `Extension<Arc<GatewayState>>` → `ws.on_upgrade(handle_events_socket)`。

**`handle_events_socket`** 流程（镜像 `handle_client_socket` `websocket.rs:241` 的 select! 循环）：
1. 首帧 Connect → authenticate → 非 Approved 关闭。loopback 检查（上面）。
2. 发 `HelloOk { v: HARNESS_API_VERSION }`。
3. `let mut sub: Option<(String, SubId, Receiver)> = None;`（当前 attach 的 session + 订阅 id + 事件 rx）。
4. `loop { tokio::select! { ... } }`：
   - 读 socket：解析 `HarnessRequest`，match：
     - `Hello { v }` → v 匹配回 `HelloOk`，不匹配 `Error`。
     - `ListSessions` → `event_bus.list()` → `SessionList`。
     - `AttachSession { session_key }` → 若已有 sub 先 `unsubscribe`；`resolve_session_key`（复用 `legion_host::routing`，同 `sessions.history` `ws_rpc.rs:539`）校验 key；`event_bus.subscribe(resolved_key)` → `(sub_id, run_id, rx)`；history = `legion_host::turn::load_session_history(...)`（`turn.rs:79`，复用）；回 `Attached { session_key, run_id, history }`；存 sub。
     - `DetachSession` → 若有 sub `unsubscribe`、清 sub、回 `Detached`。
     - `Ping` → `Pong`。
   - 若有 sub，读 `sub.rx.recv()` → 收到 `HarnessServerFrame` 写回 socket；rx 关闭（被逐出）→ 发 `Error { "subscription dropped (slow consumer)" }`、清 sub、继续循环（GUI 可重新 attach）。
5. 循环退出时若有 sub → `unsubscribe`（清理）。

attach 语义：任意合法 session_key 都可 attach（条目不存在则创建），attach 后等待未来 turn 的事件；run_id=Some 时正在跑（attach 那刻之后的 delta 实时收，之前的 delta 丢失——用 history 的 ChatMessage 兜底整条消息，符合"向前流+历史消息"）。

**Slice 3 测试**：`is_loopback_bind` 单测；handler 的握手/Hello 版本检查（用既有 ws 测试 fixture，参考 `tests/ws_tests.rs` / `tests/session_fixture.rs` 的建连方式）。

## Slice 4 — 集成测试（主验收，"测真实路径"）

`crates/legion-gateway/tests/events_test.rs`：复用 `session_fixture.rs` 起 gateway（loopback）。两个 WS 连接：
1. 连接 A：Connect → 发 `agent` RPC 触发一个 turn（用 fixture 的 agent + 一个无副作用 tool，参考现有 ws_tests 模式），拿到 run_id 和 session_key。
2. 连接 B：连 `/events` → Connect → Hello → `AttachSession { session_key }` → 断言 `Attached`（history 非空或 run_id 命中）→ 断言收到 `RunStarted`/`ToolStarted`/`ToolFinished`/`RunFinished`（按出现顺序，不 hard-code 内容，只断言事件 kind 序列）。
覆盖：attach 到正在跑的 turn 能收到剩余事件；ListSessions 返回该 session 且 status=Live。

**手动验收脚本**（写进 docs，非测试）：~15 行 `websocat`/python 连 `/events`，打印帧。先 attach，另一终端 CLI gateway 模式发消息，观察流。

## Slice 5 — 删除 HookRunner（subtraction）

完整删除面（已逐项确认）：
- 删 `crates/legion-automation/src/hooks.rs`（整文件，含内联 5 个测试）。
- `crates/legion-automation/src/lib.rs:10` 删 `pub mod hooks;`。
- `crates/legion-gateway/src/gateway.rs:12` 删 `use legion_automation::hooks::HookRunner;`；删 `gateway.rs:382-388`（GatewayStop 块）；删 `gateway.rs:501-507`（GatewayStart 块）。
- Cargo.toml 不动（`async-trait`/`dirs`/`tempfile` 其他模块仍用）。
- config 无 hook 字段（`~/.legion/hooks/` 是硬编码，非配置），无需改 config。
- 文档更新（仅 stale 修复）：`README.md:199`、`docs/design/gaps/04-breadth/automation-advanced.md:21,36`、`docs/design/agent-harness-prd.md` 相关段——改为指向新 `/events` 总线（Slice 6 一起做）。
- 行为损失：仅 `~/.legion/hooks/gateway-start.*`/`gateway-stop.*` 脚本不再触发（当前无生产 in-process Hook 注册）。这是预期的收编点。

## Slice 6 — 文档

新增 `docs/design/events-bus.md`（或填进 gaps）：`/events` 契约（事件枚举、帧格式、AttachSession 模型、loopback 约束、向前流+历史语义、版本化策略）、验收脚本、follow-up 清单。更新 `docs/design/gaps/00-overview.md` 关闭对应 gap（AGENTS.md 要求）。

## 验收闸（每个 slice 后 + 收尾）

```bash
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test  -p legion-protocol && cargo test -p legion-gateway
cargo fmt -- --check
# 收尾：cargo test --workspace --all-targets（除需 MINIMAX_API_KEY 的 e2e）
```

## 明确的 v1 局限 / follow-up（写进 docs）

- 仅 WS `agent` 路径的 run 可订阅；channel/cron/task_runner 触发的 turn 不经 `handle_agent`，暂不可见（第二个 fan-out 接线点 = 在 `channel_inbound.rs:232` 闭包加同样的 `bus.publish`，同模式可复制）。
- `ContextCompacted`/`TodoListUpdated` 枚举位预留，v1 不 emit（v1.1 接 `from_agent_payload` 的 compaction/todo 分支）。
- 非远程：`/events` 仅 loopback。远程 GUI 需后续加 session-token 授权（grilling 时已识别为默认 1 的代价）。
- ListSessions 仅活跃 session；磁盘上历史 session 不列出（需完整 key out-of-band 才能 attach）。
- 一个 `/events` 连接 attach 一个 session（v1 简化）。