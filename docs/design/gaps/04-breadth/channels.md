# Gap:Channel 生态仅 2 个(访问控制未执行)

| 字段 | 值 |
|---|---|
| 类目 | [04-breadth](./_index.md)(生态广度) |
| 优先级 | P2 |
| 工作量 | L(总体;单 channel S-M) |
| 前置依赖 | [plugin-facade](../02-missing/plugin-facade.md)(channel 通过插件接入) |
| 关联 PRD | `agent-harness-prd.md` §5 C4-C8(Slack/WhatsApp/iMessage/访问控制) |
| 关联参考 | `docs/openclaw_raw/channels.md`(27+ 平台)、`channels/bot-loop-protection.md`、`channels/access-groups.md` |
| 状态 | ✅ **已实施**(Phase A+B+C + 收尾切片,2026-07-11,见 DEVLOG;Phase D 桥接型暂不承诺) |

---

## 1. 现状证据

- **仅 2 个 channel**:`legion-channel/src/{telegram.rs, webchat.rs}`。PRD C4/C5/C6 的 Slack/WhatsApp/iMessage 全缺;无 Discord/Lark/Matrix/SMS/Email。
- **Telegram 能力受限**:无 webhook 模式(仅 long-poll)、无 reactions/typing(`telegram.rs:65-79` capabilities 全 false)。
- **WebChat media 缺陷**:`webchat.rs:103-106` 的 `send` 只存 text(虽 capabilities 声明支持 media)。
- **访问控制"假功能"** ⚠️:PRD C8 的 `dmPolicy`(open/allowlist/pairing)、`allowFrom`、groups `requireMention` 在配置示例里详尽定义,但 `grep dmPolicy|allowFrom|requireMention` **运行时零命中**——`config.channels` 仅透传 `Value` 给 provider,**无任何策略执行引擎**。这是"声明 vs 事实"最典型的案例。

**结论**:channel 接入面窄,且已声明的访问控制是空壳。对照 OpenClaw 27+ channel,差距巨大,但策略是"做精前几个 + 扩展模板",而非全抄。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:每个新 channel = 实现 `ChannelProvider` trait + 配置,不改核心(依赖 plugin-facade)。
- **P2 安全**:访问控制**默认生效**(dmPolicy/requireMention 真执行);bot-loop-protection 防 agent 互相触发死循环。
- **P3 增量**:新 channel 可选启用;Telegram/WebChat 行为不变。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:消息准入/拒绝、loop 检测产生 `tracing`。
- **P6 失败显式**:channel 断线/限流/webhook 校验失败分类处理。
- **P7 测试**:访问控制各 policy、loop 检测、各 channel inbound 解析测试。

---

## 3. 架构设计

### 3.1 优先级排序(不全抄 OpenClaw)

| 优先级 | Channel | 理由 |
|---|---|---|
| P2-高 | **Slack** | 企业协作主场景,OpenClaw/PRD 均列 P1 |
| P2-高 | **Discord** | 社区/技术场景,API 成熟 |
| P2-中 | **Lark(飞书)** | 国内企业场景 |
| P2-中 | **Matrix** | 开源/自托管场景 |
| P3 | WhatsApp/iMessage/SMS | 个人 IM,依赖第三方桥接(Baileys/BlueBubbles),复杂度高 |

### 3.2 访问控制引擎(修复"假功能")

```
InboundMessage 到达
   ▼
AccessControlEngine.evaluate(msg, policy)
   ▼
AccessDecision:
   Allow / Deny(非 allowlist/未配对) / RequireMention(群组未 @)
   ▼ Allow
route_inbound_to_runtime(现有)
```

借鉴 OpenClaw `access-groups.md` + `bot-loop-protection.md`。

### 3.3 bot-loop-protection(借鉴)

防 agent A 回复触发 agent B 回复再触发 A 的死循环:跟踪 (channel, peer) 最近 N 条 outbound,检测自回环;命中则静默丢弃 + 告警。

### 3.4 capabilities 声明驱动

```rust
pub struct ChannelCapabilities {
    pub reactions: bool,
    pub typing_indicators: bool,
    pub media_send: bool,   // WebChat 当前 false(修复)
    pub reply: bool,
    pub edit: bool,
}
```
agent 行为按 capabilities 降级(无 typing 则不发 typing)。

---

## 4. 接口设计(Rust)

### 4.1 访问控制(`legion-channel` 新增)

```rust
use crate::{InboundMessage, PeerId};

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessPolicy {
    pub dm_policy: DmPolicy,             // 默认 Allowlist
    pub allowlist: Vec<PeerId>,
    pub groups: GroupPolicy,
}
#[derive(Debug, Clone, Copy, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy { Open, #[default] Allowlist, Pairing }

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GroupPolicy {
    pub require_mention: bool,           // 默认 true
    pub allowlist: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum AccessDecision { Allow, Deny(DenyReason), RequireMention }
#[derive(Debug, Clone)]
pub enum DenyReason { NotInAllowlist, NotPaired, BotLoop }

pub trait AccessControlEngine: Send + Sync {
    fn evaluate(&self, msg: &InboundMessage, policy: &AccessPolicy) -> AccessDecision;
}

pub struct BotLoopGuard {
    recent_outbound: HashMap<(String, PeerId), Vec<std::time::Instant>>,
}
impl BotLoopGuard {
    pub fn check(&self, msg: &InboundMessage) -> bool;   // true=放行,false=疑似 loop
    pub fn record_outbound(&mut self, channel: &str, peer: &PeerId);
}
```

### 4.2 新 channel 模板(以 Slack 为例)

```rust
// crates/legion-channel/src/slack.rs (或独立插件 crate)
pub struct SlackProvider { config: SlackConfig, client: SlackClient, caps: ChannelCapabilities }

#[async_trait]
impl ChannelProvider for SlackProvider {
    fn id(&self) -> &str { "slack" }
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities { reactions: true, typing_indicators: true,
                              media_send: true, reply: true, edit: true }
    }
    async fn start(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}
```

### 4.3 配置 schema

```jsonc
{
  "channels": {
    "slack": {
      "enabled": true,
      "botToken": "${SLACK_BOT_TOKEN}",
      "signingSecret": "${SLACK_SIGNING_SECRET}",
      "access": { "dmPolicy": "allowlist", "allowlist": ["U123"], "groups": { "requireMention": true } }
    }
  }
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-channel/src/lib.rs:20-90` | `route_inbound_to_runtime` 前插入 `AccessControlEngine.evaluate` + `BotLoopGuard.check`。 |
| `legion-core/src/config.rs` | `ChannelConfig` 增 `access: AccessPolicy`(默认 Allowlist + requireMention)。 |
| 新增 `legion-channel/src/slack.rs`、`discord.rs` 等 | 各自实现 `ChannelProvider`(或走 plugin-facade 独立 crate)。 |
| `legion-channel/src/webchat.rs:103-106` | 修复 media_send(实际支持 media 注入)。 |
| `legion-channel/src/telegram.rs:65-79` | 补 reactions/typing capabilities + webhook 模式(可选)。 |

---

## 6. 风险与权衡

### 6.1 访问控制默认值(安全)
当前"假功能"意味着任何 DM 都能触发 agent。**修复后默认 `dmPolicy: Allowlist` + `requireMention: true`**(最小权限);`Open` 需显式配置 + 告警。

### 6.2 bot-loop-protection 误杀
过于激进会丢弃正常消息。**缓解**:仅检测"短窗口内同 (channel,peer) 高频 outbound"模式;阈值可配;命中告警而非静默(可配静默)。

### 6.3 channel 实现成本
每个 channel 需处理:inbound 解析、media 下载/上传、rate limit、webhook 签名校验、重连。**Slack/Discord API 成熟,S 工作量;WhatsApp/iMessage 需第三方桥接,L 工作量**。故优先 Slack/Discord。

### 6.4 capabilities 驱动降级
agent 不应假设所有 channel 支持 reactions/typing。`ChannelCapabilities` 让 agent 工具(如 reactions)按能力降级,而非硬崩。

### 6.5 因地制宜:webhook vs long-poll
Telegram 当前 long-poll。Slack/Discord 用 webhook(event push)。两种模式都要支持,`ChannelProvider::start` 抽象差异。

### 6.6 桥接型 channel 的运维风险
WhatsApp(Baileys)/iMessage(BlueBubbles)依赖第三方非官方桥接,稳定性/合规风险。**列为 P3,文档警示**,非默认推荐。

---

## 7. 实现路线图

### 阶段 A(Phase C,~0.5 人周):访问控制引擎(修复假功能) — ✅ 已落地(2026-07-11,见 DEVLOG)
1. 新建 `legion-channel/src/access.rs`:`AccessPolicy`/`DmPolicy`(open/allowlist/pairing)/`GroupPolicy`(requireMention 默认 true + groups allowlist)+ `AccessDecision`/`DenyReason` + `evaluate` + `policy_for`(从 `channels.<id>.access` 解析,缺省=最小权限);`BotLoopGuard`(按 (channel,peer) 跟踪 outbound 回复,窗口内超 `max_replies` 则拒绝后续 inbound,gateway 接线为 60s/5 次)。✅
2. `route_inbound_to_runtime` 接入:approval 回复之后、resolver 之前评估;Deny/RequireMention/BotLoop 均 `tracing` 记录并 return;回复发送成功后 `record_outbound`。gateway inbound 路由传入共享 guard。✅
3. **验收**:非 allowlist DM 被拒(`dm_allowlist_denies_strangers_by_default`);群组未 @ 被 RequireMention 拦截(`group_requires_mention_by_default`);loop 检测生效(`bot_loop_guard_trips_after_max_replies`);policy 解析/默认/各策略分支共 9 测试。✅
4. **行为变化(安全修复)**:无 `access` 配置时 DM 默认拒绝(空 allowlist)——此前任何 DM 都能触发 agent。恢复旧行为需显式 `channels.<id>.access.dmPolicy: "open"`;WS `agent` 方法(已认证客户端)不走此路径,不受影响。

### 阶段 B(Phase C,~1 人周):Slack + Discord — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `SlackProvider`(`legion-channel/src/slack.rs`,新建):**Socket Mode**(`apps.connections.open` 拿 wss url → 纯函数 `parse_socket_envelope`/`parse_message_event`;events_api 需 ack `{"envelope_id": id}`;`disconnect` → Reconnect;外层重连循环 + 5s 退避);跳过所有 `subtype`/`bot_id`(防 bot 互喷);`channel_type=="im"` → Direct 否则 Group;`thread_ts` → `peer.thread_id`;`app_mention` → `is_mentioned`;send 走 `chat.postMessage`(reply_to/peer.thread_id → thread_ts,检查 `ok:false`)。✅
2. `DiscordProvider`(`legion-channel/src/discord.rs`,新建):**Gateway WS**(`GET /gateway/bot` → `?v=10&encoding=json`;HELLO(op10)配心跳 + IDENTIFY(op2,intents=37377);READY 存 bot user id 用于 mention 判定;MESSAGE_CREATE → 纯函数 `parse_message_create`(跳过 `author.bot`,有 `guild_id` → Group,attachments 按 `content_type` image/ → Image 否则 Document);op7/op9/断线 → 外层重连(不做 RESUME,代码注释已注明);心跳 op1 带最后 seq)。send 走 `POST /channels/{id}/messages`。✅
3. 接线:`lib.rs` 导出;`plugins.rs` 加 `SlackPlugin`/`DiscordPlugin` 包装(`system:channel-slack`/`system:channel-discord`)并注册进 PluginRegistry(回复路径依赖);`SystemPlugins` 加字段;`gateway.rs` 按 `channels.slack/discord` 配置启动/停止。✅
4. **验收**:14 个纯函数单测(config 解析、envelope/message 解析、bot/subtype 跳过、mention 判定、DM/Group kind、Ack/Reconnect 分支、attachments 提取)全过;clippy/fmt 干净;全量 26 suite 全绿。**诚实备注:live 网络路径(WS 连接/心跳/重连/真实 send)本环境无凭据,未 E2E 实测。**
5. 未做(后续切片):WebChat media_send 修复、Telegram reactions/typing、`ChannelCapabilities` 驱动降级。

### 阶段 C(Phase C,~1 人周):Lark + Matrix — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `LarkProvider`(`legion-channel/src/lark.rs`,新建,~880 行):**飞书长连接 WebSocket**——`POST /callback/ws/endpoint`(AppID/AppSecret)取 wss URL;手写 pbbp2.Frame protobuf 最小编解码(无 prost);CONTROL 帧 init/ping→pong;DATA 帧 header `type=event` → 纯函数 `parse_event_payload`(仅 `im.message.receive_v1` + `sender_type=="user"`;p2p→Direct/group→Group;text content JSON 解析;mention 按 botOpenId 或 mentions 非空)并回 `{"code":200}` ack 防重投;gzip 帧 warn 一次后丢弃(workspace 无 flate2,仍回 ack);send 走 `im/v1/messages?receive_id_type=chat_id`,tenant_access_token 带缓存(60s 过期余量)。8 个单测(含 frame 编解码 round-trip)。✅
2. `MatrixProvider`(`legion-channel/src/matrix.rs`,新建,~610 行):**client-server sync 长轮询**——start 时 whoami 解析 own user_id(可配置);`GET /sync?timeout=30000&since=` 循环存 next_batch;纯函数 `parse_sync_response`(m.room.message + 非自发;m.text/m.image/m.file;account_data 的 m.direct 映射判定 Direct;body.contains(own_user_id) 作 mention);send 走 `PUT /rooms/{id}/send/m.room.message/{txn}`。8 个单测。✅
3. 接线:`LarkPlugin`/`MatrixPlugin` 包装(`system:channel-lark`/`system:channel-matrix`)注册进 PluginRegistry;`SystemPlugins` 加字段;gateway 按 `channels.lark/matrix` 启停。✅
4. **验收**:16 个纯函数单测全过;clippy/fmt 干净;全量 26 suite 全绿(legion-channel lib 52 测试)。**live 网络路径因无凭据未 E2E 实测。**
5. 未做(后续切片):WebChat media_send 修复、Telegram reactions/typing、`ChannelCapabilities` 驱动降级;Lark 富文本/卡片消息、gzip 帧、RESUME 级重连恢复;Matrix E2EE/端到端加密。

### 收尾切片(2026-07-11,见 DEVLOG)— ✅ 已落地
1. **WebChat media_send 复核**:`WebChatProvider::send` 原样入队完整 `OutboundMessage`(含 `media`),capabilities 已声明四类 media——后端链路本就 pass-through,无"假功能"可修。✅(复核)
2. **Telegram typing + reactions**:`ChannelProvider` trait 加默认 no-op 方法 `send_typing`/`add_reaction`(所有现有 provider 与外部插件零改动兼容);Telegram 实现 `sendChatAction`/`setMessageReaction`,capabilities 翻为 `reactions/typing: true`。✅
3. **capabilities 驱动降级**:`route_inbound_to_runtime` 在 access 通过后按 capabilities 门控——typing:true 时 spawn 4s 周期 typing 循环(watch 信号在回复发送/run 结束时停止),reactions:true 时收消息即回 👀;false 的 channel 完全不 spawn,零开销不崩;provider 查找从两次合并为一次。✅
4. **验收**:8 个新测试(wiremock 校验 sendChatAction/setMessageReaction 路径与 body、NotStarted/非法 chat_id 分支;typing/reactions 门控与降级);legion-channel lib 60 测试;clippy/fmt 干净。

### 阶段 D(P3):桥接型
- WhatsApp/iMessage/SMS 评估第三方桥接稳定性与合规。暂不承诺(不阻塞本 gap 收官)。

---

## 8. 验收标准

- [x] 访问控制引擎真执行:`dmPolicy: allowlist` 拒绝非 allowlist DM(测试)。(Phase A)
- [x] 群组 `requireMention: true` 拦截未 @ 消息(测试)。(Phase A)
- [x] bot-loop-protection 检测自回环(测试)。(Phase A)
- [x] Slack + Discord 双向文本消息 + 解析/mention/thread/attachments(纯函数单测 14 个;live E2E 因无凭据未实测)。(Phase B)
- [x] Lark + Matrix inbound 解析 + outbound send(纯函数单测 16 个,含 pbbp2 frame 编解码 round-trip;live E2E 因无凭据未实测)。(Phase C)
- [x] WebChat media_send 实际支持 media(复核:`send` 入队完整 `OutboundMessage` 含 media,capabilities 已声明;后端链路本就 pass-through)。(收尾切片)
- [x] 新增 channel = 实现 `ChannelProvider` + 配置,不改核心(plugin-facade 验证:Slack/Discord/Lark/Matrix 均为 system plugin 包装)。(Phase B+C)
- [x] `ChannelCapabilities` 驱动降级:`route_inbound_to_runtime` 按 typing/reactions 门控 spawn(无该能力的 channel 零开销不崩;测试覆盖)。(收尾切片)
- [x] Telegram typing(sendChatAction 4s 周期)+ reactions(setMessageReaction 👀)落地(wiremock 测试)。(收尾切片)
- [x] 默认 `dmPolicy: Allowlist`(最小权限,安全)。(Phase A)
- [x] 准入/拒绝/loop 有 `tracing`。(Phase A)
- [x] `AGENTS.md` 更新 channel 章节(声明访问控制真执行 + 新 channel)。(Phase A+B+C + 收尾切片)

---

*下一个 gap:[`providers.md`](./providers.md) · 返回类目:[`_index.md`](./_index.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
