# `/events` 外部事件总线

> 状态:**已实现(v1)**。这是 Legion 的出口订阅总线——让外部工具(以及未来的 GUI)被动接收 session 的生命周期/工具/文本流式事件,只观察不拦截。

## 1. 它解决什么

Legion 此前的事件能力是碎片化的:内部 `RunEvent` 流是单消费者(被 host 抽干)、`/ws` 协议只把事件发给**发起该 run 的连接**,而 `HookRunner` 是进程级脚本钩子且大半事件是死代码。

`/events` 提供一个**独立、版本化、可被任意连接订阅**的出口:任何通过鉴权的 `/events` 连接都能 `AttachSession` 到某个 session,收到该 session 此后所有 run 的事件流。设计上**不动内部协议**——fan-out 与现有 `/ws` 转发并行发生。

## 2. 传输与鉴权

- **端点**:WebSocket,路径 `/events`(与 `/ws` 同一 axum router,`gateway.rs` 的 `router()`)。
- **握手**:首帧必须是 `connect`(`ConnectParams` + `AuthCreds`),复用 `/ws` 的 `authenticate()`。成功后服务端发 harness `hello_ok`。
- **loopback 硬约束**:`/events` 允许 attach 到**别人发起的** session,因此**仅在 loopback bind 上启用**。handler 入口检查 `state.config.gateway.bind_host`;非 loopback 直接关闭并返回错误。这与 AGENTS.md 的"非 loopback reject `auth:none`"安全条款一致。

## 3. 版本化

`HARNESS_API_VERSION = 1`(`legion-protocol::harness`)。客户端首帧 `connect` 后可发 `hello { v }`,服务端版本不匹配返回 `error`。schema 独立于内部 `CURRENT_PROTOCOL_REVISION`,内部 `RunEvent` 改动不影响它——**隔离层是 `HarnessEvent::from_agent_payload`**。

## 4. 协议契约

### 客户端 → 服务端(`HarnessRequest`,`#[serde(tag = "type")]`)

| 帧 | 字段 | 说明 |
|---|---|---|
| `hello` | `v: u32` | 版本握手(可选,首帧 connect 之后) |
| `list_sessions` | — | 列出活跃(注册表内有条目的)session |
| `attach_session` | `sessionKey: String` | 订阅某 session 的实时事件流;替换本连接之前的 attach |
| `detach_session` | — | 取消订阅 |
| `ping` | — | 心跳 |

`sessionKey` 是完整 7 段 key:`agent:<agent_id>:<scope>:<channel>:<account_id>:<peer_kind>:<peer_id>`,经 `resolve_session_key` 解析(同 `sessions.history` RPC)。

### 服务端 → 客户端(`HarnessServerFrame`)

| 帧 | 说明 |
|---|---|
| `hello_ok { v }` | 握手应答 |
| `session_list { sessions }` | `HarnessSessionSummary { sessionKey, agentId, peerId, runId?, status }`,status=`live`/`idle` |
| `attached { sessionKey, runId?, history }` | attach 应答。`history` 是持久化的 `ChatMessage` 列表(单一真相源);`runId` 存在表示当前有 turn 在跑 |
| `detached` | detach 应答 |
| `event { event }` | 一个 `HarnessEvent`(见下) |
| `error { message }` | 错误(连接保持,除非是握手/loopback 致命错误) |
| `pong` | 心跳应答 |

### `HarnessEvent` 枚举(v1 发射)

| 变体 | 来源 `RunEvent` | 携带 |
|---|---|---|
| `run_started` | `Lifecycle{Start}` | `sessionKey`, `runId` |
| `run_finished` | `Lifecycle{End}` | `sessionKey`, `runId` |
| `run_errored` | `Lifecycle{Error}` | `sessionKey`, `runId`, `error` |
| `assistant_text_delta` | `AssistantDelta` | `runId`, `delta`(文本片段,需累加) |
| `tool_started` | `ToolStart` | `runId`, `toolCall{id,name,arguments}` |
| `tool_finished` | `ToolEnd` | `runId`, `toolCall`, `result{content,isError}`, `canonicalMeta?` |

v1 **schema 预留但不发射**:`context_compacted`、`todo_list_updated`(枚举位已存在,留待 v1.1 接 `from_agent_payload` 的 compaction/todo 分支)。

字段命名 `camelCase`(per AGENTS.md)。

## 5. 语义:向前流 + 历史消息

- **历史**:`attach_session` 返回 `history` = 该 session 持久化的 `ChatMessage`(经 `load_session_history`,与 `sessions.history` RPC 同源)。**不额外持久化 RunEvent**——ChatMessage 是唯一落盘真相(符合 AGENTS.md 的 single source of truth)。
- **实时**:`attach` 之后的事件实时推送。
- **attach 到正在跑的 turn**:能收到 attach 之后的事件;attach 之前的 delta 不重放,但 `history` 里的 ChatMessage 提供完整消息兜底。因此 GUI 重连后不会丢失"整条消息",只会丢失"那次 turn 的逐字流式过程"。

## 6. 背压与慢消费者

每个订阅者用 `tokio::sync::mpsc::channel(512)`。`publish` 对所有事件 `try_send`:

- 通道满(`Full`)→ **逐出该订阅者**(其 `rx` 关闭,handler 发 `error{subscription dropped}`),GUI 可重新 attach 拿 fresh history 自愈。
- 通道关闭(`Closed`)→ 同样逐出。

这样慢消费者被断开而非拖垮 gateway;`run_finished` 等关键事件不会因慢消费者阻塞。

## 7. 数据流

```
run_loop (emit RunEvent)
  → drive_run_stream (legion-host) 把 RunEvent 转成 agent event payload
  → handle_agent 的 emit 闭包 (legion-gateway/ws_rpc.rs):
       ├─ tx.send(frame)                        ← 既有:发给发起方 /ws(不动)
       └─ HarnessEvent::from_agent_payload()    ← 新增:薄映射
            → event_bus.publish(session_key, ev) ← fan-out 给 /events 订阅者
  → /events 连接的订阅者 rx → 写回 socket
```

**关键不变量**:`drive_run_stream` 签名零改动;内部 `/ws` 协议零改动;fan-out 完全在 gateway 侧闭包里。

## 8. 关键代码位置

- **类型 + 映射层**:`crates/legion-protocol/src/harness.rs`(`HarnessEvent::from_agent_payload`)。
- **EventBus + /events handler**:`crates/legion-gateway/src/events.rs`(`EventBus`、`events_handler`、`handle_events_socket`)。
- **fan-out 接线**:`crates/legion-gateway/src/ws_rpc.rs` `handle_agent` 的 emit 闭包 + `register_run`/`end_run`。
- **GatewayState 字段**:`crates/legion-gateway/src/websocket.rs`(`event_bus`)。
- **路由 + loopback 工具**:`crates/legion-gateway/src/gateway.rs`(`.route("/events", ...)`);`is_loopback_bind` 在 `websocket.rs`。
- **验收测试**:`crates/legion-gateway/tests/events_test.rs`(端到端:`run_started → tool_started → tool_finished → run_finished`)。

## 9. 手动验收

```bash
# 终端 1:起一个 loopback gateway
legion gateway start   # bindHost 127.0.0.1

# 终端 2:订阅 /events(websocat)
websocat ws://127.0.0.1:<port>/events
# 先发 connect 帧,再发 attach_session,观察事件流

# 终端 3:触发一个 turn
legion ...   # 或通过 /ws 发 agent RPC
```

## 10. v1 局限 / Follow-up

- **仅 WS `agent` 路径的 run 可订阅**:channel/cron/task_runner 触发的 turn 不经 `handle_agent`,暂不可见。第二个 fan-out 接线点 = 在 `legion-host/src/channel_inbound.rs` 的 emit 闭包加同样的 `bus.publish`(模式可复制)。
- **`context_compacted`/`todo_list_updated` 不发射**:v1.1 接入映射层。
- **仅 loopback**:远程 GUI 需后续加 session-token 授权(attach 时校验发起方签发的 token)。
- **ListSessions 仅活跃 session**:磁盘上历史 session 不列出(需完整 key out-of-band 才能 attach)。
- **一个 `/events` 连接 attach 一个 session**:v1 简化;多 session 订阅 / 过滤订阅(agent/tag)留待后续。

## 11. 设计决策记录(来自 grilling)

1. **纯出口订阅型**,不做 block/deny 语义——不碰 `ApprovalNotifier`/policy。
2. **独立 `/events` 端点 + 独立 semver schema**,与内部 `/ws` 的 `WsFrame` 隔离。
3. **废弃 HookRunner**,收编进统一事件词汇表(减法,符合 AGENTS.md)。
4. **向前流 + 历史 ChatMessage**,不额外持久化 RunEvent(单一真相源)。
5. **attach 到单个 session**(ListSessions + AttachSession,学 jcode-harness-api)。
6. **ListSessions 仅活跃 session**(注册表里有条目的)。
7. **schema = 稳定对外枚举 + 薄映射层**,从已构建的 agent payload 映射,不改 `drive_run_stream` 签名。
8. **非 loopback 硬性拒绝 /events**。
9. **TokenDelta 在 v1 且实时 fan-out**(GUI 后续做实时聊天 UI 的前提)。
