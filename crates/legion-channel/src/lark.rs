use crate::util::{
    Lifecycle, StopPolicy, cfg_required, cfg_str, cfg_str_or, lark_envelope, send_json,
    ws_reconnect_loop,
};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use legion_plugin_sdk::channel::{
    ChannelCapabilities, ChannelError, ChannelProvider, InboundMessage, OutboundMessage, Peer,
    PeerKind, Sender,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Frame method: control messages (init / ping / pong).
const METHOD_CONTROL: i32 = 0;
/// Frame method: data messages (events + their acknowledgements).
const METHOD_DATA: i32 = 1;

/// Events whose `create_time` is older than this on arrival are treated as
/// redeliveries. Lark retries deliveries it considers unacknowledged on a
/// backoff schedule spanning hours (a 2.7h-late redelivery was observed in
/// production), and the in-memory deduper starts empty on process restart —
/// this check is the cross-restart guard. Normal deliveries arrive within a
/// second or two of creation, so 120s leaves ample margin.
const STALE_EVENT_THRESHOLD: Duration = Duration::from_secs(120);

/// True when the event's `create_time` (ms since epoch, verbatim from the
/// event header) is older than [`STALE_EVENT_THRESHOLD`]. Missing or
/// unparseable timestamps are treated as fresh and fall through to the
/// deduper.
fn is_stale_redelivery(timestamp: &str, now_ms: u64) -> bool {
    let Ok(create_ms) = timestamp.parse::<u64>() else {
        return false;
    };
    now_ms.saturating_sub(create_ms) > STALE_EVENT_THRESHOLD.as_millis() as u64
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Built-in Lark (Feishu) channel provider using the long-connection
/// WebSocket mode (self-built app, no public callback URL needed).
///
/// The wire protocol is the `pbbp2.Frame` protobuf message, for which this
/// module implements a minimal hand-rolled encoder/decoder (no prost).
///
/// Reconnect simplification: on reconnect we re-request a fresh WebSocket URL
/// from the endpoint instead of reusing the server-issued `conn_id` /
/// `reconnect_interval` / `reconnect_nonce` resume parameters. Events missed
/// during the reconnect window are lost (acceptable for an MVP channel).
///
/// gzip-compressed event payloads are not supported (the workspace has no
/// flate2 dependency); a warning is logged once and the frame is dropped.
///
/// NOTE: the live socket path (endpoint / frames / reconnect) is covered only
/// by pure-function unit tests; it has not been exercised against the real
/// Lark API in this environment. The same applies to the interactive approval
/// card path (`send_approval_card` and the `card.action.trigger` callback
/// parsing): covered by unit tests, not verified against live Lark.
#[derive(Debug)]
pub struct LarkProvider {
    lifecycle: Lifecycle<LarkConfig>,
    token_cache: Mutex<Option<(String, Instant)>>,
    /// Pending "processing" reactions keyed by message_id, so the reply path
    /// can remove the exact reaction it added (`reaction_id` is returned by
    /// the add API and required by the delete API).
    reactions: Mutex<HashMap<String, String>>,
}

#[derive(Debug, Clone)]
struct LarkConfig {
    app_id: String,
    app_secret: String,
    bot_open_id: Option<String>,
    base_url: String,
    account_id: String,
}

/// Lark delivers events "at least once": even after a successful ACK, a
/// transient hiccup on its side can redeliver the same message. Per the
/// official `im.message.receive_v1` docs, dedupe on `message_id` (it is stable
/// across redeliveries; `event_id` is not). This is an in-memory, per-channel
/// guard only — entries expire so the map stays bounded.
struct EventDeduper {
    seen: HashMap<String, Instant>,
    ttl: Duration,
}

impl EventDeduper {
    fn new(ttl: Duration) -> Self {
        Self {
            seen: HashMap::new(),
            ttl,
        }
    }

    /// Records `message_id` and returns `true` if it had not been seen within
    /// the TTL window (i.e. this delivery should be processed). Returns `false`
    /// for a duplicate, in which case the caller should ACK without processing.
    fn check(&mut self, message_id: &str) -> bool {
        let now = Instant::now();
        let deadline = now - self.ttl;
        // Opportunistic GC: drop anything older than the TTL so the map never
        // grows without bound (no LRU dependency needed).
        self.seen.retain(|_, seen_at| *seen_at > deadline);

        match self.seen.insert(message_id.to_string(), now) {
            None => true,
            Some(prev) if prev <= deadline => true, // had aged out; treat as fresh
            Some(_) => false,
        }
    }
}

impl LarkProvider {
    pub fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            token_cache: Mutex::new(None),
            reactions: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for LarkProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelProvider for LarkProvider {
    fn channel_id(&self) -> &str {
        "lark"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            text: true,
            media: vec![],
            group: true,
            thread: false,
            reactions: true,
            typing: false,
            buttons: true,
        }
    }

    async fn start(
        &self,
        config: Value,
        inbound_tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), ChannelError> {
        let cfg = parse_config(config)?;

        let running = self.lifecycle.running.clone();
        let http = self.lifecycle.http.clone();
        let account_id = cfg.account_id.clone();
        let task_cfg = cfg.clone();

        self.lifecycle
            .begin(cfg, async move {
                socket_loop(&http, &task_cfg, inbound_tx, running).await;
            })
            .await;

        tracing::info!(channel = "lark", account = %account_id, "Lark channel started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChannelError> {
        self.lifecycle.stop(StopPolicy::Abort).await;
        *self.token_cache.lock().await = None;
        self.reactions.lock().await.clear();
        tracing::info!(channel = "lark", "Lark channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        let cfg = self.lifecycle.config().await?;

        let token = self.tenant_token(&cfg).await?;
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=chat_id",
            cfg.base_url
        );
        let peer = &message.peer.id;
        let text_len = message.text.as_deref().map(str::len).unwrap_or(0);
        let payload = json!({
            "receive_id": message.peer.id,
            "msg_type": "text",
            "content": json!({ "text": message.text.unwrap_or_default() }).to_string(),
        });

        let response =
            send_json(self.lifecycle.http.post(&url).bearer_auth(&token), &payload).await?;
        lark_envelope(response, "lark send message").await?;

        tracing::info!(peer = %peer, text_len, "lark outbound reply sent");
        Ok(())
    }

    async fn add_reaction(
        &self,
        _peer: &Peer,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), ChannelError> {
        let cfg = self.lifecycle.config().await?;
        let token = self.tenant_token(&cfg).await?;
        let emoji_type = lark_emoji_type(emoji);
        let url = format!(
            "{}/open-apis/im/v1/messages/{message_id}/reactions",
            cfg.base_url
        );
        let payload = json!({ "reaction_type": { "emoji_type": emoji_type } });
        let response =
            send_json(self.lifecycle.http.post(&url).bearer_auth(&token), &payload).await?;
        let body = lark_envelope(response, "lark add reaction").await?;

        // The add API returns the reaction id; the delete API requires it.
        if let Some(reaction_id) = body
            .pointer("/data/reaction_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        {
            self.reactions
                .lock()
                .await
                .insert(message_id.to_string(), reaction_id);
        }
        tracing::info!(message_id, emoji_type, "lark reaction added");
        Ok(())
    }

    async fn remove_reaction(
        &self,
        _peer: &Peer,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), ChannelError> {
        // The add path may not have stored a reaction_id (e.g. the API response
        // lacked it, or add never ran). Nothing to remove in that case.
        let Some(reaction_id) = self.reactions.lock().await.remove(message_id) else {
            return Ok(());
        };
        let cfg = self.lifecycle.config().await?;
        let token = self.tenant_token(&cfg).await?;
        let url = format!(
            "{}/open-apis/im/v1/messages/{message_id}/reactions/{reaction_id}",
            cfg.base_url
        );
        let response = self
            .lifecycle
            .http
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed(e.to_string()))?;
        lark_envelope(response, "lark remove reaction").await?;
        tracing::info!(
            message_id,
            emoji_type = lark_emoji_type(emoji),
            "lark reaction removed"
        );
        Ok(())
    }

    async fn send_approval_card(
        &self,
        peer: &Peer,
        tool: &str,
        prompt_id: &str,
    ) -> Result<bool, ChannelError> {
        let cfg = self.lifecycle.config().await?;
        let token = self.tenant_token(&cfg).await?;
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=chat_id",
            cfg.base_url
        );
        let payload = approval_card_payload(&peer.id, tool, prompt_id);
        let response =
            send_json(self.lifecycle.http.post(&url).bearer_auth(&token), &payload).await?;
        lark_envelope(response, "lark send approval card").await?;

        tracing::info!(peer = %peer.id, tool, prompt_id, "lark approval card sent");
        Ok(true)
    }
}

impl LarkProvider {
    /// Fetch (or reuse the cached) tenant access token. The cache is refreshed
    /// 60 seconds before the server-reported expiry.
    async fn tenant_token(&self, cfg: &LarkConfig) -> Result<String, ChannelError> {
        {
            let cache = self.token_cache.lock().await;
            if let Some((token, expiry)) = &*cache
                && *expiry > Instant::now()
            {
                return Ok(token.clone());
            }
        }

        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            cfg.base_url
        );
        let response = send_json(
            self.lifecycle.http.post(&url),
            &json!({ "app_id": cfg.app_id, "app_secret": cfg.app_secret }),
        )
        .await?;

        let body = lark_envelope(response, "lark tenant_access_token").await?;

        let token = body
            .get("tenant_access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ChannelError::SendFailed("tenant_access_token response missing token".into())
            })?
            .to_string();
        let expire_secs = body
            .get("expire")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200)
            .saturating_sub(60);
        *self.token_cache.lock().await = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(expire_secs),
        ));
        Ok(token)
    }
}

fn parse_config(config: Value) -> Result<LarkConfig, ChannelError> {
    let app_id = cfg_required(&config, &["appId", "app_id"], "lark appId is required")?;

    let app_secret = cfg_required(
        &config,
        &["appSecret", "app_secret"],
        "lark appSecret is required",
    )?;

    let bot_open_id = cfg_str(&config, &["botOpenId", "bot_open_id"]).map(str::to_string);

    let base_url = cfg_str_or(&config, &["baseUrl", "base_url"], "https://open.feishu.cn");

    let account_id = cfg_str_or(&config, &["accountId", "account_id"], "default");

    Ok(LarkConfig {
        app_id,
        app_secret,
        bot_open_id,
        base_url,
        account_id,
    })
}

// ---------------------------------------------------------------------------
// pbbp2.Frame minimal protobuf codec (hand-rolled, no prost).
//
// message Header { string key = 1; string value = 2; }
// message Frame {
//   uint64 seqid = 1; uint64 logid = 2; int32 service = 3; int32 method = 4;
//   repeated Header headers = 5; string payload_encoding = 6;
//   string payload_type = 7; bytes payload = 8;
// }
// ---------------------------------------------------------------------------

/// A decoded `pbbp2.Frame`.
#[derive(Debug, Clone, PartialEq)]
struct Frame {
    seqid: u64,
    logid: u64,
    service: i32,
    method: i32,
    headers: Vec<(String, String)>,
    payload_encoding: String,
    payload_type: String,
    payload: Vec<u8>,
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn encode_tag(field: u64, wire_type: u64, out: &mut Vec<u8>) {
    encode_varint((field << 3) | wire_type, out);
}

fn encode_varint_field(field: u64, value: u64, out: &mut Vec<u8>) {
    encode_tag(field, 0, out);
    encode_varint(value, out);
}

fn encode_len_delimited(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
    encode_tag(field, 2, out);
    encode_varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn encode_string_field(field: u64, value: &str, out: &mut Vec<u8>) {
    encode_len_delimited(field, value.as_bytes(), out);
}

fn encode_frame(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint_field(1, frame.seqid, &mut out);
    encode_varint_field(2, frame.logid, &mut out);
    // int32 is sign-extended to 64 bits on the wire (two's complement varint).
    encode_varint_field(3, frame.service as i64 as u64, &mut out);
    encode_varint_field(4, frame.method as i64 as u64, &mut out);
    for (key, value) in &frame.headers {
        let mut inner = Vec::new();
        encode_string_field(1, key, &mut inner);
        encode_string_field(2, value, &mut inner);
        encode_len_delimited(5, &inner, &mut out);
    }
    encode_string_field(6, &frame.payload_encoding, &mut out);
    encode_string_field(7, &frame.payload_type, &mut out);
    encode_len_delimited(8, &frame.payload, &mut out);
    out
}

fn decode_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let byte = *data.get(*pos)?;
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn decode_len_delimited<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = decode_varint(data, pos)? as usize;
    let slice = data.get(*pos..(*pos).checked_add(len)?)?;
    *pos += len;
    Some(slice)
}

fn decode_string(data: &[u8], pos: &mut usize) -> Option<String> {
    let bytes = decode_len_delimited(data, pos)?;
    String::from_utf8(bytes.to_vec()).ok()
}

fn decode_header(data: &[u8]) -> Option<(String, String)> {
    let mut pos = 0;
    let mut key = String::new();
    let mut value = String::new();
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        match (tag >> 3, tag & 7) {
            (1, 2) => key = decode_string(data, &mut pos)?,
            (2, 2) => value = decode_string(data, &mut pos)?,
            (_, 0) => {
                decode_varint(data, &mut pos)?;
            }
            (_, 2) => {
                decode_len_delimited(data, &mut pos)?;
            }
            _ => return None,
        }
    }
    Some((key, value))
}

fn decode_frame(data: &[u8]) -> Option<Frame> {
    let mut frame = Frame {
        seqid: 0,
        logid: 0,
        service: 0,
        method: 0,
        headers: Vec::new(),
        payload_encoding: String::new(),
        payload_type: String::new(),
        payload: Vec::new(),
    };
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        match (tag >> 3, tag & 7) {
            (1, 0) => frame.seqid = decode_varint(data, &mut pos)?,
            (2, 0) => frame.logid = decode_varint(data, &mut pos)?,
            (3, 0) => frame.service = decode_varint(data, &mut pos)? as i64 as i32,
            (4, 0) => frame.method = decode_varint(data, &mut pos)? as i64 as i32,
            (5, 2) => {
                let inner = decode_len_delimited(data, &mut pos)?;
                frame.headers.push(decode_header(inner)?);
            }
            (6, 2) => frame.payload_encoding = decode_string(data, &mut pos)?,
            (7, 2) => frame.payload_type = decode_string(data, &mut pos)?,
            (8, 2) => frame.payload = decode_len_delimited(data, &mut pos)?.to_vec(),
            // Skip unknown fields we can step over; bail on unsupported types.
            (_, 0) => {
                decode_varint(data, &mut pos)?;
            }
            (_, 2) => {
                decode_len_delimited(data, &mut pos)?;
            }
            _ => return None,
        }
    }
    Some(frame)
}

// ---------------------------------------------------------------------------
// Event payload parsing.
// ---------------------------------------------------------------------------

/// Strip Lark `@`-mention placeholders from message text.
///
/// In group messages Lark embeds mention placeholders shaped `@_user_1`,
/// `@_user_2`, ... (and `@_all`) directly in `content.text`. Left in place
/// these defeat prefix-based detection such as `approve:<prompt>` approval
/// replies, and leak noisy tokens into the agent's view. We remove each
/// placeholder token and collapse the surrounding whitespace, leaving the
/// real text behind (`@_user_1 approve:prompt-0` -> `approve:prompt-0`).
fn strip_lark_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("@_") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        // Consume an identifier body: alphanumerics and underscores. This
        // covers `user_1`, `user_42`, `all`, etc.
        let end = after
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        // If nothing valid follows `@_`, it is not a mention placeholder
        // (e.g. an email or plain text); keep it verbatim.
        if end == 0 {
            out.push('@');
            out.push('_');
            rest = after;
        } else {
            rest = &after[end..];
        }
    }
    out.push_str(rest);
    // Collapse runs of whitespace left behind by removed placeholders and trim.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Map an emoji character used internally to Lark's `emoji_type` identifier.
/// Lark's reaction API takes a name (e.g. `OneSecond`), not the Unicode glyph.
/// The full set of valid names comes from the official emoji reference; values
/// outside it are rejected with "reaction type is invalid".
fn lark_emoji_type(emoji: &str) -> &str {
    match emoji {
        "⏳" => "OneSecond",
        "👀" => "EYES",
        "👍" => "THUMBSUP",
        "🎉" => "PARTY",
        "✅" => "CheckMark",
        "🙏" => "THANKS",
        other => other,
    }
}

/// Convert a Lark `im.message.receive_v1` event payload into an inbound
/// message. Pure function for unit testing.
fn parse_event_payload(
    payload: &[u8],
    bot_open_id: Option<&str>,
    account_id: &str,
) -> Option<InboundMessage> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    let header = value.get("header")?;
    if header.get("event_type").and_then(|v| v.as_str()) != Some("im.message.receive_v1") {
        return None;
    }

    let event = value.get("event")?;
    let sender = event.get("sender")?;
    // Skip non-human senders (bot messages, including our own echoes).
    if sender.get("sender_type").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }

    let message = event.get("message")?;
    let chat_id = message.get("chat_id")?.as_str()?;
    let chat_type = message
        .get("chat_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let kind = if chat_type == "p2p" {
        PeerKind::Direct
    } else {
        PeerKind::Group
    };

    let message_type = message
        .get("message_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Text messages carry a JSON-encoded string in `content`.
    let text = if message_type == "text" {
        message
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|content| {
                content
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(strip_lark_mentions)
            })
    } else {
        None
    };

    let mentions = message.get("mentions").and_then(|v| v.as_array());
    let is_mentioned = match (mentions, bot_open_id) {
        (Some(list), Some(bot_id)) => {
            !list.is_empty()
                && list
                    .iter()
                    .any(|m| m.pointer("/id/open_id").and_then(|v| v.as_str()) == Some(bot_id))
        }
        (Some(list), None) => !list.is_empty(),
        (None, _) => false,
    };

    Some(InboundMessage {
        channel: "lark".into(),
        account_id: account_id.into(),
        peer: Peer {
            kind,
            id: chat_id.into(),
            name: None,
            thread_id: None,
        },
        sender: Sender {
            id: sender
                .pointer("/sender_id/open_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .into(),
            display_name: None,
            username: None,
        },
        message_id: message
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        text,
        media: vec![],
        reply_to: None,
        // Milliseconds since epoch, stored verbatim.
        timestamp: header
            .get("create_time")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        is_mentioned,
        ambient: false,
        guild_id: None,
        team_id: None,
    })
}

/// Build the message-create request body for an interactive tool-approval
/// card. The card carries two buttons in one action row; each button's
/// `value` is a JSON object the `card.action.trigger` callback echoes back
/// verbatim, which [`parse_card_action`] turns into an approval reply.
/// Pure function for unit testing.
fn approval_card_payload(receive_id: &str, tool: &str, prompt_id: &str) -> Value {
    let card = json!({
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": format!("工具审批: {tool}") },
            "template": "blue",
        },
        "elements": [
            {
                "tag": "div",
                "text": {
                    "tag": "plain_text",
                    "content": format!("工具 '{tool}' 需要审批，请选择："),
                },
            },
            {
                "tag": "action",
                "actions": [
                    {
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": "批准" },
                        "type": "primary",
                        "value": { "approval": "approve", "prompt_id": prompt_id },
                    },
                    {
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": "拒绝" },
                        "type": "danger",
                        "value": { "approval": "deny", "prompt_id": prompt_id },
                    },
                ],
            },
        ],
    });
    json!({
        "receive_id": receive_id,
        "msg_type": "interactive",
        "content": card.to_string(),
    })
}

/// Convert a Lark `card.action.trigger` callback payload into a synthetic
/// inbound approval reply (`approve:<prompt_id>` / `deny:<prompt_id>`), so the
/// existing approval-reply interception in `channel_inbound.rs` resolves it
/// without any new wiring. Pure function for unit testing.
///
/// Detection is deliberately tolerant: a payload counts as a card action when
/// its `header.event_type` is `card.action.trigger`, or when the frame arrived
/// with header type `card` and the payload carries both `event.action` and
/// `event.operator`. Callbacks from foreign cards (their `action.value` lacks
/// our `approval`/`prompt_id` keys) return `None` and are ignored.
fn parse_card_action(
    payload: &[u8],
    frame_type: Option<&str>,
    account_id: &str,
) -> Option<InboundMessage> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    let is_card_event =
        value.pointer("/header/event_type").and_then(|v| v.as_str()) == Some("card.action.trigger");
    let looks_like_action =
        value.pointer("/event/action").is_some() && value.pointer("/event/operator").is_some();
    if !(is_card_event || (frame_type == Some("card") && looks_like_action)) {
        return None;
    }

    let action_value = value.pointer("/event/action/value")?;
    let prompt_id = action_value.get("prompt_id").and_then(|v| v.as_str())?;
    let decision = action_value
        .get("approval")
        .or_else(|| action_value.get("decision"))
        .and_then(|v| v.as_str())?;
    let allow = match decision {
        "approve" => true,
        "deny" => false,
        _ => return None,
    };

    let operator_open_id = value
        .pointer("/event/operator/open_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let peer = match value
        .pointer("/event/context/open_chat_id")
        .and_then(|v| v.as_str())
    {
        Some(chat_id) => Peer {
            kind: PeerKind::Group,
            id: chat_id.into(),
            name: None,
            thread_id: None,
        },
        None => Peer {
            kind: PeerKind::Direct,
            id: operator_open_id.into(),
            name: None,
            thread_id: None,
        },
    };

    Some(InboundMessage {
        channel: "lark".into(),
        account_id: account_id.into(),
        peer,
        sender: Sender {
            id: operator_open_id.into(),
            display_name: None,
            username: None,
        },
        message_id: format!("card-{}", legion_core::util::next_id()),
        text: Some(format!(
            "{}:{}",
            if allow { "approve" } else { "deny" },
            prompt_id
        )),
        media: vec![],
        reply_to: None,
        // Milliseconds since epoch when present, verbatim.
        timestamp: value
            .pointer("/header/create_time")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        is_mentioned: false,
        ambient: false,
        guild_id: None,
        team_id: None,
    })
}

// ---------------------------------------------------------------------------
// Connection plumbing.
// ---------------------------------------------------------------------------
async fn open_ws_endpoint(http: &reqwest::Client, cfg: &LarkConfig) -> Result<String, String> {
    let url = format!("{}/callback/ws/endpoint", cfg.base_url);
    let response = http
        .post(&url)
        .json(&json!({ "AppID": cfg.app_id, "AppSecret": cfg.app_secret }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    if body.get("code").and_then(|v| v.as_i64()) != Some(0) {
        let msg = body
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_error");
        return Err(format!("lark ws endpoint failed: {msg}"));
    }

    body.pointer("/data/URL")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "lark ws endpoint response missing data.URL".to_string())
}

/// Outer reconnect loop: re-request a fresh endpoint URL on every reconnect
/// (see the reconnect simplification note on [`LarkProvider`]).
async fn socket_loop(
    http: &reqwest::Client,
    cfg: &LarkConfig,
    inbound_tx: mpsc::Sender<InboundMessage>,
    running: Arc<AtomicBool>,
) {
    // Dedup state lives here (outside the per-connection loop) so it survives
    // reconnects: a redelivery can arrive on a fresh connection. Lark retries
    // deliveries it considers unacknowledged on a backoff schedule spanning
    // hours (a 2.7h-late redelivery was observed in production), so the window
    // must cover that, not just the immediate post-ACK duplicate.
    let mut deduper = EventDeduper::new(Duration::from_secs(24 * 60 * 60));
    let open_http = http.clone();
    let open_cfg = cfg.clone();
    let serve_cfg = cfg.clone();
    let serve_running = running.clone();
    ws_reconnect_loop(
        "lark",
        &running,
        &mut deduper,
        move || {
            let http = open_http.clone();
            let cfg = open_cfg.clone();
            Box::pin(async move { open_ws_endpoint(&http, &cfg).await })
        },
        |deduper: &mut EventDeduper, url: String| {
            let cfg = serve_cfg.clone();
            let inbound_tx = inbound_tx.clone();
            let running = serve_running.clone();
            Box::pin(
                async move { run_connection(&url, &cfg, &inbound_tx, &running, deduper).await },
            )
        },
    )
    .await;
}

/// One long-connection session: read frames, answer pings, route events, and
/// acknowledge every DATA frame so Lark does not redeliver it.
async fn run_connection(
    url: &str,
    cfg: &LarkConfig,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    running: &Arc<AtomicBool>,
    deduper: &mut EventDeduper,
) -> Result<(), String> {
    let (ws, _) = connect_async(url).await.map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws.split();

    while let Some(raw) = read.next().await {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        let bytes = match raw {
            Ok(Message::Binary(bytes)) => bytes,
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => continue,
            Err(err) => return Err(err.to_string()),
        };

        let Some(frame) = decode_frame(bytes.as_ref()) else {
            tracing::warn!("lark: dropping undecodable frame");
            continue;
        };

        match frame.method {
            METHOD_CONTROL => handle_control(&mut write, &frame).await?,
            METHOD_DATA => {
                handle_data(&mut write, &frame, cfg, inbound_tx, deduper).await?;
            }
            _ => {}
        }
    }

    Ok(())
}

async fn handle_control<S>(write: &mut S, frame: &Frame) -> Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
{
    let Ok(payload) = serde_json::from_slice::<Value>(&frame.payload) else {
        return Ok(());
    };
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("init") => {
            let conn_id = payload
                .get("conn_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            tracing::info!(conn_id, "lark long connection established");
        }
        Some("ping") => {
            let pong = Frame {
                seqid: frame.seqid,
                logid: frame.logid,
                service: frame.service,
                method: frame.method,
                headers: vec![("type".into(), "pong".into())],
                payload_encoding: String::new(),
                payload_type: String::new(),
                payload: json!({ "type": "pong" }).to_string().into_bytes(),
            };
            write
                .send(Message::binary(encode_frame(&pong)))
                .await
                .map_err(|_| "failed to send lark pong frame".to_string())?;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_data<S>(
    write: &mut S,
    frame: &Frame,
    cfg: &LarkConfig,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    deduper: &mut EventDeduper,
) -> Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
{
    let started = Instant::now();
    let frame_type = frame
        .headers
        .iter()
        .find(|(key, _)| key == "type")
        .map(|(_, value)| value.as_str());

    if frame_type == Some("event") || frame_type == Some("card") {
        if let Some(msg) = parse_card_action(&frame.payload, frame_type, &cfg.account_id) {
            // Interactive approval card button click: synthesize the same
            // `approve:<id>` / `deny:<id>` reply the text flow produces, so
            // the existing inbound interception resolves it.
            tracing::info!(
                sender = %msg.sender.id,
                peer = %msg.peer.id,
                text = %msg.text.as_deref().unwrap_or(""),
                "lark approval card action received"
            );
            if inbound_tx.send(msg).await.is_err() {
                tracing::warn!(
                    "lark inbound channel closed; dropping card action without ack (lark will redeliver)"
                );
                return Ok(());
            }
        } else if frame_type == Some("event") {
            if frame.payload_encoding == "gzip" {
                // The workspace has no flate2 dependency; warn once and drop.
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "lark: gzip-encoded event payloads are not supported; dropping frame"
                    );
                }
            } else if let Some(msg) =
                parse_event_payload(&frame.payload, cfg.bot_open_id.as_deref(), &cfg.account_id)
            {
                tracing::info!(
                    message_id = %msg.message_id,
                    sender = %msg.sender.id,
                    peer = %msg.peer.id,
                    text_len = msg.text.as_deref().map(str::len).unwrap_or(0),
                    "lark inbound event received"
                );
                // Dedupe before forwarding: Lark's "at least once" delivery can
                // redeliver the same message even after a successful ACK. We still
                // ACK duplicates below so Lark stops retrying, but we never forward
                // a duplicate (which would produce a duplicate reply).
                if is_stale_redelivery(&msg.timestamp, now_ms()) {
                    // The deduper starts empty on process restart, so a late
                    // redelivery would slip through it. create_time is stable
                    // across redeliveries, so an already-old event is a
                    // redelivery: ACK it below, but never process it again.
                    tracing::info!(
                        message_id = %msg.message_id,
                        create_time = %msg.timestamp,
                        "lark stale event skipped (redelivery after restart)"
                    );
                } else if deduper.check(&msg.message_id) {
                    if inbound_tx.send(msg).await.is_err() {
                        tracing::warn!(
                            "lark inbound channel closed; dropping event without ack (lark will redeliver)"
                        );
                        return Ok(());
                    }
                } else {
                    tracing::info!(
                        message_id = %msg.message_id,
                        "lark duplicate event deduplicated"
                    );
                }
            }
        } else {
            // Foreign card action (not one of our approval cards): ignore it.
            tracing::debug!("lark card frame ignored (not an approval callback)");
        }

        // Acknowledge every DATA frame, otherwise Lark redelivers the event.
        // Mirror the official SDK: echo the request headers back (the server
        // correlates the ACK via message_id/trace_id; without them it treats
        // the delivery as unacknowledged and keeps retrying) and append
        // biz_rt with the processing time in milliseconds.
        let mut ack_headers = frame.headers.clone();
        ack_headers.push(("biz_rt".into(), started.elapsed().as_millis().to_string()));
        let ack = Frame {
            seqid: frame.seqid,
            logid: frame.logid,
            service: frame.service,
            method: frame.method,
            headers: ack_headers,
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: json!({ "code": 200, "headers": null, "data": null })
                .to_string()
                .into_bytes(),
        };
        write
            .send(Message::binary(encode_frame(&ack)))
            .await
            .map_err(|_| "failed to send lark data ack frame".to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn frame(method: i32, headers: Vec<(String, String)>, payload: &[u8]) -> Frame {
        Frame {
            seqid: 42,
            logid: 7,
            service: 1,
            method,
            headers,
            payload_encoding: "json".into(),
            payload_type: "event".into(),
            payload: payload.to_vec(),
        }
    }

    fn event_payload(chat_type: &str, mentions: Value) -> Vec<u8> {
        json!({
            "schema": "2.0",
            "header": {
                "event_type": "im.message.receive_v1",
                "create_time": "1700000000123",
            },
            "event": {
                "sender": {
                    "sender_id": { "open_id": "ou_user1" },
                    "sender_type": "user",
                },
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_1",
                    "chat_type": chat_type,
                    "message_type": "text",
                    "content": "{\"text\":\"hello legion\"}",
                    "mentions": mentions,
                },
            },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn frame_codec_round_trip() {
        let original = frame(
            METHOD_DATA,
            vec![("type".into(), "event".into()), ("k2".into(), "v2".into())],
            b"{\"hello\":\"world\"}",
        );
        let decoded = decode_frame(&encode_frame(&original)).expect("frame should decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn frame_codec_round_trip_empty_payload_and_headers() {
        let original = frame(METHOD_CONTROL, vec![], b"");
        let decoded = decode_frame(&encode_frame(&original)).expect("frame should decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_frame_rejects_garbage() {
        assert_eq!(decode_frame(&[0xff, 0xff, 0xff]), None);
        // Unknown wire type (1 = fixed64 is skipped? no: unsupported) -> None.
        assert_eq!(decode_frame(&[0x09, 0x00]), None);
    }

    #[test]
    fn parse_config_requires_app_credentials() {
        let err = parse_config(json!({})).unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));

        let err = parse_config(json!({ "appId": "cli_1" })).unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));

        let cfg = parse_config(json!({
            "appId": "cli_1",
            "appSecret": "sec_1",
        }))
        .unwrap();
        assert_eq!(cfg.account_id, "default");
        assert_eq!(cfg.base_url, "https://open.feishu.cn");
        assert_eq!(cfg.bot_open_id, None);

        let cfg = parse_config(json!({
            "app_id": "cli_2",
            "app_secret": "sec_2",
            "bot_open_id": "ou_bot",
            "account_id": "work",
        }))
        .unwrap();
        assert_eq!(cfg.app_id, "cli_2");
        assert_eq!(cfg.bot_open_id.as_deref(), Some("ou_bot"));
        assert_eq!(cfg.account_id, "work");
    }

    #[test]
    fn parses_p2p_message_as_direct() {
        let payload = event_payload("p2p", json!([]));
        let msg = parse_event_payload(&payload, Some("ou_bot"), "acc1").unwrap();
        assert_eq!(msg.channel, "lark");
        assert_eq!(msg.account_id, "acc1");
        assert_eq!(msg.peer.kind, PeerKind::Direct);
        assert_eq!(msg.peer.id, "oc_1");
        assert_eq!(msg.sender.id, "ou_user1");
        assert_eq!(msg.message_id, "om_1");
        assert_eq!(msg.text, Some("hello legion".into()));
        assert_eq!(msg.timestamp, "1700000000123");
        assert!(!msg.is_mentioned);
        assert_eq!(msg.team_id, None);
        assert_eq!(msg.guild_id, None);
    }

    #[test]
    fn parses_group_message_and_mention_rules() {
        let payload = event_payload("group", json!([{ "id": { "open_id": "ou_bot" } }]));

        // Bot open id known: mention list must contain it.
        let msg = parse_event_payload(&payload, Some("ou_bot"), "default").unwrap();
        assert_eq!(msg.peer.kind, PeerKind::Group);
        assert!(msg.is_mentioned);

        let msg = parse_event_payload(&payload, Some("ou_other"), "default").unwrap();
        assert!(!msg.is_mentioned);

        // Bot open id unknown: any non-empty mention list counts.
        let msg = parse_event_payload(&payload, None, "default").unwrap();
        assert!(msg.is_mentioned);

        // Empty mention list never counts.
        let payload = event_payload("group", json!([]));
        let msg = parse_event_payload(&payload, None, "default").unwrap();
        assert!(!msg.is_mentioned);
    }

    #[test]
    fn skips_non_user_senders_and_other_event_types() {
        let mut value: Value = serde_json::from_slice(&event_payload("p2p", json!([]))).unwrap();
        value["event"]["sender"]["sender_type"] = json!("app");
        let payload = value.to_string().into_bytes();
        assert!(parse_event_payload(&payload, None, "default").is_none());

        let mut value: Value = serde_json::from_slice(&event_payload("p2p", json!([]))).unwrap();
        value["header"]["event_type"] = json!("im.chat.member.user.added_v1");
        let payload = value.to_string().into_bytes();
        assert!(parse_event_payload(&payload, None, "default").is_none());
    }

    #[test]
    fn non_text_message_type_yields_no_text() {
        let mut value: Value = serde_json::from_slice(&event_payload("group", json!([]))).unwrap();
        value["event"]["message"]["message_type"] = json!("image");
        value["event"]["message"]["content"] = json!("{\"image_key\":\"img_1\"}");
        let payload = value.to_string().into_bytes();
        let msg = parse_event_payload(&payload, None, "default").unwrap();
        assert_eq!(msg.text, None);
        assert!(msg.media.is_empty());
    }

    #[test]
    fn event_deduper_first_delivery_is_processed() {
        let mut deduper = EventDeduper::new(Duration::from_secs(600));
        assert!(deduper.check("om_1")); // unseen -> process
        assert!(deduper.check("om_2")); // different id -> process
    }

    #[test]
    fn event_deduper_redelivery_is_dropped() {
        let mut deduper = EventDeduper::new(Duration::from_secs(600));
        assert!(deduper.check("om_dup")); // first delivery
        assert!(!deduper.check("om_dup")); // redelivery within TTL -> drop
    }

    #[test]
    fn event_deduper_evicts_after_ttl() {
        // A short TTL lets the entry age out without a long wait.
        let mut deduper = EventDeduper::new(Duration::from_millis(20));
        assert!(deduper.check("om_ttl"));
        std::thread::sleep(Duration::from_millis(60));
        // The expired entry is reaped on the next check, so the message is
        // treated as fresh again.
        assert!(deduper.check("om_ttl"));
    }

    #[test]
    fn stale_redelivery_detection() {
        let now = 1_700_000_000_123_u64;
        let fresh = (now - 5_000).to_string(); // 5s old -> process
        assert!(!is_stale_redelivery(&fresh, now));

        let old = (now - 10 * 60 * 1000).to_string(); // 10min old -> skip
        assert!(is_stale_redelivery(&old, now));

        // Clock skew (event "from the future") must not be treated as stale.
        let future = (now + 60_000).to_string();
        assert!(!is_stale_redelivery(&future, now));

        // Missing/unparseable timestamps fall through to the deduper.
        assert!(!is_stale_redelivery("", now));
        assert!(!is_stale_redelivery("not-a-number", now));
    }

    #[test]
    fn strip_lark_mentions_removes_bot_mention_prefix() {
        // The approval-reply case: the leading @bot placeholder must vanish so
        // `approve:<prompt>` is recognized by prefix matching.
        assert_eq!(
            strip_lark_mentions("@_user_1 approve:prompt-0"),
            "approve:prompt-0"
        );
    }

    #[test]
    fn strip_lark_mentions_handles_multiple_and_all() {
        assert_eq!(strip_lark_mentions("@_user_1 @_user_2 你好"), "你好");
        assert_eq!(strip_lark_mentions("@_all 开会了"), "开会了");
    }

    #[test]
    fn strip_lark_mentions_leaves_plain_at_alone() {
        // A bare `@_` with no identifier body, or a normal `@handle`, must not
        // be mangled.
        assert_eq!(strip_lark_mentions("a@b.com"), "a@b.com");
        assert_eq!(strip_lark_mentions("@_ 你好"), "@_ 你好");
        assert_eq!(strip_lark_mentions("plain text"), "plain text");
    }

    #[test]
    fn parses_group_message_strips_mention_from_text() {
        // A group text whose content carries the @bot placeholder should
        // arrive with the placeholder removed.
        let mut value: Value = serde_json::from_slice(&event_payload("group", json!([]))).unwrap();
        value["event"]["message"]["content"] = json!("{\"text\":\"@_user_1 approve:prompt-9\"}");
        let payload = value.to_string().into_bytes();
        let msg = parse_event_payload(&payload, Some("ou_bot"), "default").unwrap();
        assert_eq!(msg.text.as_deref(), Some("approve:prompt-9"));
    }

    #[test]
    fn lark_emoji_type_maps_known_glyphs() {
        assert_eq!(lark_emoji_type("⏳"), "OneSecond");
        assert_eq!(lark_emoji_type("👀"), "EYES");
        assert_eq!(lark_emoji_type("👍"), "THUMBSUP");
        assert_eq!(lark_emoji_type("✅"), "CheckMark");
    }

    #[test]
    fn lark_emoji_type_passes_through_unknown() {
        // An unknown emoji is returned verbatim so we never silently substitute.
        assert_eq!(lark_emoji_type("🦀"), "🦀");
    }

    /// Build a provider whose config already points at `base_url`, skipping the
    /// socket `start()` path so reaction HTTP calls can be mocked directly.
    fn started_provider(base_url: String) -> LarkProvider {
        let cfg = LarkConfig {
            app_id: "cli_x".into(),
            app_secret: "sec".into(),
            bot_open_id: None,
            base_url,
            account_id: "default".into(),
        };
        LarkProvider {
            lifecycle: Lifecycle {
                http: reqwest::Client::new(),
                running: Arc::new(AtomicBool::new(false)),
                config: Mutex::new(Some(cfg)),
                task: Mutex::new(None),
            },
            token_cache: Mutex::new(None),
            reactions: Mutex::new(HashMap::new()),
        }
    }

    fn dm_peer(id: &str) -> Peer {
        Peer {
            kind: PeerKind::Direct,
            id: id.into(),
            name: None,
            thread_id: None,
        }
    }

    #[tokio::test]
    async fn add_reaction_posts_emoji_and_stores_reaction_id() {
        let server = MockServer::start().await;
        // tenant_access_token is fetched before any IM call.
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "code": 0, "tenant_access_token": "tok" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/open-apis/im/v1/messages/om_1/reactions"))
            .and(body_json(
                json!({ "reaction_type": { "emoji_type": "OneSecond" } }),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "code": 0, "data": { "reaction_id": "r_42" } })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = started_provider(server.uri());
        provider
            .add_reaction(&dm_peer("oc_1"), "om_1", "⏳")
            .await
            .expect("add_reaction should succeed");
        assert_eq!(
            provider
                .reactions
                .lock()
                .await
                .get("om_1")
                .map(String::as_str),
            Some("r_42")
        );
    }

    #[tokio::test]
    async fn remove_reaction_deletes_stored_reaction_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "code": 0, "tenant_access_token": "tok" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/open-apis/im/v1/messages/om_1/reactions/r_42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "code": 0 })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = started_provider(server.uri());
        provider
            .reactions
            .lock()
            .await
            .insert("om_1".into(), "r_42".into());
        provider
            .remove_reaction(&dm_peer("oc_1"), "om_1", "⏳")
            .await
            .expect("remove_reaction should succeed");
        // The stored id must be consumed.
        assert!(provider.reactions.lock().await.get("om_1").is_none());
    }

    #[tokio::test]
    async fn remove_reaction_without_prior_add_is_noop() {
        // No stored reaction_id -> no DELETE call, no error.
        let provider = started_provider("http://unused".into());
        provider
            .remove_reaction(&dm_peer("oc_1"), "om_9", "⏳")
            .await
            .expect("remove_reaction should be a no-op when nothing was added");
    }

    #[test]
    fn approval_card_payload_is_interactive_with_both_buttons() {
        let payload = approval_card_payload("oc_1", "exec", "prompt-3");
        assert_eq!(payload["receive_id"], "oc_1");
        assert_eq!(payload["msg_type"], "interactive");

        let card: Value = serde_json::from_str(payload["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            card.pointer("/header/title/content")
                .and_then(|v| v.as_str()),
            Some("工具审批: exec")
        );
        let actions = card["elements"][1]["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["type"], "primary");
        assert_eq!(
            actions[0]["value"],
            json!({ "approval": "approve", "prompt_id": "prompt-3" })
        );
        assert_eq!(actions[1]["type"], "danger");
        assert_eq!(
            actions[1]["value"],
            json!({ "approval": "deny", "prompt_id": "prompt-3" })
        );
    }

    #[test]
    fn parse_card_action_maps_approve_click_to_approval_reply() {
        // Official card.action.trigger callback shape (schema 2.0).
        let payload = json!({
            "schema": "2.0",
            "header": {
                "event_type": "card.action.trigger",
                "create_time": "1700000000123",
            },
            "event": {
                "operator": { "open_id": "ou_xxx" },
                "action": {
                    "tag": "button",
                    "value": { "approval": "approve", "prompt_id": "prompt-3" },
                },
                "context": { "open_chat_id": "oc_yyy" },
            },
        })
        .to_string()
        .into_bytes();

        let msg = parse_card_action(&payload, Some("card"), "acc1").unwrap();
        assert_eq!(msg.channel, "lark");
        assert_eq!(msg.account_id, "acc1");
        assert_eq!(msg.text.as_deref(), Some("approve:prompt-3"));
        assert_eq!(msg.sender.id, "ou_xxx");
        assert_eq!(msg.peer.kind, PeerKind::Group);
        assert_eq!(msg.peer.id, "oc_yyy");
        // The synthesized text must be recognized by the approval-reply parser.
        assert_eq!(
            crate::parse_approval_reply(msg.text.as_deref().unwrap()),
            Some(("prompt-3", true))
        );
    }

    #[test]
    fn parse_card_action_tolerates_card_frame_without_event_type_header() {
        // ws protocol variant: frame header type is "card", payload has no
        // schema-2.0 header block; detection falls back to action+operator.
        let payload = json!({
            "event": {
                "operator": { "open_id": "ou_xxx" },
                "action": {
                    "tag": "button",
                    "value": { "decision": "deny", "prompt_id": "prompt-8" },
                },
            },
        })
        .to_string()
        .into_bytes();

        let msg = parse_card_action(&payload, Some("card"), "default").unwrap();
        assert_eq!(msg.text.as_deref(), Some("deny:prompt-8"));
        // No chat context: fall back to a direct peer with the operator id.
        assert_eq!(msg.peer.kind, PeerKind::Direct);
        assert_eq!(msg.peer.id, "ou_xxx");

        // Without the "card" frame type and without the event_type header the
        // payload is not treated as a card action at all.
        assert!(parse_card_action(&payload, Some("event"), "default").is_none());
    }

    #[test]
    fn parse_card_action_ignores_foreign_card_actions() {
        let payload = json!({
            "schema": "2.0",
            "header": { "event_type": "card.action.trigger" },
            "event": {
                "operator": { "open_id": "ou_xxx" },
                "action": { "tag": "button", "value": { "other": "card" } },
                "context": { "open_chat_id": "oc_yyy" },
            },
        })
        .to_string()
        .into_bytes();
        assert!(parse_card_action(&payload, Some("card"), "default").is_none());

        // Unknown decision value is also ignored.
        let payload = json!({
            "header": { "event_type": "card.action.trigger" },
            "event": {
                "operator": { "open_id": "ou_xxx" },
                "action": { "value": { "approval": "maybe", "prompt_id": "prompt-1" } },
            },
        })
        .to_string()
        .into_bytes();
        assert!(parse_card_action(&payload, Some("card"), "default").is_none());
    }

    #[tokio::test]
    async fn send_approval_card_posts_interactive_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "code": 0, "tenant_access_token": "tok" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/open-apis/im/v1/messages"))
            .and(body_json(approval_card_payload("oc_1", "exec", "prompt-9")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "code": 0 })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = started_provider(server.uri());
        let sent = provider
            .send_approval_card(&dm_peer("oc_1"), "exec", "prompt-9")
            .await
            .expect("send_approval_card should succeed");
        assert!(sent);
    }
}
