use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use legion_plugin_sdk::channel::{
    ChannelCapabilities, ChannelError, ChannelProvider, InboundMessage, OutboundMessage, Peer,
    PeerKind, Sender,
};
use serde_json::{Value, json};
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
/// Lark API in this environment.
#[derive(Debug)]
pub struct LarkProvider {
    http: reqwest::Client,
    config: Mutex<Option<LarkConfig>>,
    running: Arc<AtomicBool>,
    token_cache: Mutex<Option<(String, Instant)>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
struct LarkConfig {
    app_id: String,
    app_secret: String,
    bot_open_id: Option<String>,
    base_url: String,
    account_id: String,
}

impl LarkProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            config: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            token_cache: Mutex::new(None),
            task: Mutex::new(None),
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
        }
    }

    async fn start(
        &self,
        config: Value,
        inbound_tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), ChannelError> {
        let cfg = parse_config(config)?;
        *self.config.lock().await = Some(cfg.clone());
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let http = self.http.clone();
        let account_id = cfg.account_id.clone();

        let handle = tokio::spawn(async move {
            socket_loop(&http, &cfg, inbound_tx, running).await;
        });

        *self.task.lock().await = Some(handle);
        tracing::info!(channel = "lark", account = %account_id, "Lark channel started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChannelError> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.task.lock().await.take() {
            handle.abort();
        }
        *self.config.lock().await = None;
        *self.token_cache.lock().await = None;
        tracing::info!(channel = "lark", "Lark channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        let cfg = self
            .config
            .lock()
            .await
            .clone()
            .ok_or(ChannelError::NotStarted)?;

        let token = self.tenant_token(&cfg).await?;
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=chat_id",
            cfg.base_url
        );
        let payload = json!({
            "receive_id": message.peer.id,
            "msg_type": "text",
            "content": json!({ "text": message.text.unwrap_or_default() }).to_string(),
        });

        let response = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed(e.to_string()))?;

        let body: Value = response
            .json()
            .await
            .map_err(|e| ChannelError::SendFailed(e.to_string()))?;
        if body.get("code").and_then(|v| v.as_i64()) != Some(0) {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error");
            return Err(ChannelError::SendFailed(format!(
                "lark send message rejected: {msg}"
            )));
        }

        Ok(())
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
        let response = self
            .http
            .post(&url)
            .json(&json!({ "app_id": cfg.app_id, "app_secret": cfg.app_secret }))
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed(e.to_string()))?;

        let body: Value = response
            .json()
            .await
            .map_err(|e| ChannelError::SendFailed(e.to_string()))?;
        if body.get("code").and_then(|v| v.as_i64()) != Some(0) {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error");
            return Err(ChannelError::SendFailed(format!(
                "lark tenant_access_token rejected: {msg}"
            )));
        }

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
    let app_id = config
        .get("appId")
        .or_else(|| config.get("app_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ChannelError::InvalidConfig("lark appId is required".into()))?
        .to_string();

    let app_secret = config
        .get("appSecret")
        .or_else(|| config.get("app_secret"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ChannelError::InvalidConfig("lark appSecret is required".into()))?
        .to_string();

    let bot_open_id = config
        .get("botOpenId")
        .or_else(|| config.get("bot_open_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let base_url = config
        .get("baseUrl")
        .or_else(|| config.get("base_url"))
        .and_then(|v| v.as_str())
        .unwrap_or("https://open.feishu.cn")
        .to_string();

    let account_id = config
        .get("accountId")
        .or_else(|| config.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

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
                    .map(str::to_string)
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
    while running.load(Ordering::SeqCst) {
        match open_ws_endpoint(http, cfg).await {
            Ok(url) => {
                if let Err(err) = run_connection(&url, cfg, &inbound_tx, &running).await {
                    tracing::warn!(error = %err, "lark socket connection ended");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to open lark socket connection");
            }
        }

        if !running.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

/// One long-connection session: read frames, answer pings, route events, and
/// acknowledge every DATA frame so Lark does not redeliver it.
async fn run_connection(
    url: &str,
    cfg: &LarkConfig,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    running: &Arc<AtomicBool>,
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
                handle_data(&mut write, &frame, cfg, inbound_tx).await?;
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
) -> Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
{
    let frame_type = frame
        .headers
        .iter()
        .find(|(key, _)| key == "type")
        .map(|(_, value)| value.as_str());

    if frame_type == Some("event") {
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
            && inbound_tx.send(msg).await.is_err()
        {
            return Ok(());
        }

        // Acknowledge every DATA frame, otherwise Lark redelivers the event.
        let ack = Frame {
            seqid: frame.seqid,
            logid: frame.logid,
            service: frame.service,
            method: frame.method,
            headers: Vec::new(),
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: json!({ "code": 200, "data": {} }).to_string().into_bytes(),
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
}
