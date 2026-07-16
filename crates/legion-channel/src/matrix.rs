use async_trait::async_trait;
use legion_plugin_sdk::channel::{
    ChannelCapabilities, ChannelError, ChannelProvider, InboundMessage, Media, OutboundMessage,
    Peer, PeerKind, Sender,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::{Mutex, mpsc};

/// Built-in Matrix channel provider using the client-server `/sync` long poll.
///
/// Requires a pre-issued access token (e.g. from an Element login or an
/// application service). `userId` is resolved via `whoami` when not configured.
///
/// NOTE: the live sync path (polling / token rotation) is covered only by
/// pure-function unit tests; it has not been exercised against a real
/// homeserver in this environment.
#[derive(Debug)]
pub struct MatrixProvider {
    http: reqwest::Client,
    config: Mutex<Option<MatrixConfig>>,
    running: Arc<AtomicBool>,
    txn_counter: Arc<AtomicU64>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
struct MatrixConfig {
    homeserver: String,
    access_token: String,
    account_id: String,
    user_id: Option<String>,
}

impl MatrixProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            config: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            txn_counter: Arc::new(AtomicU64::new(1)),
            task: Mutex::new(None),
        }
    }
}

impl Default for MatrixProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelProvider for MatrixProvider {
    fn channel_id(&self) -> &str {
        "matrix"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            text: true,
            media: vec!["image".into(), "document".into()],
            group: true,
            thread: false,
            reactions: false,
            typing: false,
        }
    }

    async fn start(
        &self,
        config: Value,
        inbound_tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), ChannelError> {
        let mut cfg = parse_config(config)?;
        if cfg.user_id.is_none() {
            cfg.user_id = Some(fetch_user_id(&self.http, &cfg).await?);
        }
        *self.config.lock().await = Some(cfg.clone());
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let http = self.http.clone();
        let account_id = cfg.account_id.clone();

        let handle = tokio::spawn(async move {
            sync_loop(&http, &cfg, inbound_tx, running).await;
        });

        *self.task.lock().await = Some(handle);
        tracing::info!(channel = "matrix", account = %account_id, "Matrix channel started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChannelError> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.task.lock().await.take() {
            handle.abort();
        }
        *self.config.lock().await = None;
        tracing::info!(channel = "matrix", "Matrix channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        let cfg = self
            .config
            .lock()
            .await
            .clone()
            .ok_or(ChannelError::NotStarted)?;

        let txn_id = self.txn_counter.fetch_add(1, Ordering::Relaxed);
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            cfg.homeserver, message.peer.id, txn_id
        );
        let payload = json!({
            "msgtype": "m.text",
            "body": message.text.unwrap_or_default(),
        });

        let response = self
            .http
            .put(&url)
            .bearer_auth(&cfg.access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".into());
            return Err(ChannelError::SendFailed(format!(
                "matrix send message failed: {status} {body}"
            )));
        }

        Ok(())
    }
}

fn parse_config(config: Value) -> Result<MatrixConfig, ChannelError> {
    let homeserver = config
        .get("homeserver")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ChannelError::InvalidConfig("matrix homeserver is required".into()))?
        .trim_end_matches('/')
        .to_string();

    let access_token = config
        .get("accessToken")
        .or_else(|| config.get("access_token"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ChannelError::InvalidConfig("matrix accessToken is required".into()))?
        .to_string();

    let account_id = config
        .get("accountId")
        .or_else(|| config.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    let user_id = config
        .get("userId")
        .or_else(|| config.get("user_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Ok(MatrixConfig {
        homeserver,
        access_token,
        account_id,
        user_id,
    })
}

/// Convert a `/sync` response into inbound messages. Pure function for unit
/// testing. Only joined-room timeline `m.room.message` events from other
/// users are routed; `m.text` / `m.image` / `m.file` are understood.
fn parse_sync_response(
    response: &Value,
    own_user_id: &str,
    account_id: &str,
) -> Vec<InboundMessage> {
    let direct_rooms = direct_room_ids(response);
    let mut out = Vec::new();

    let Some(joined) = response.pointer("/rooms/join").and_then(|v| v.as_object()) else {
        return out;
    };

    for (room_id, room) in joined {
        let is_direct = direct_rooms.contains(room_id.as_str());
        let Some(events) = room.pointer("/timeline/events").and_then(|v| v.as_array()) else {
            continue;
        };

        for event in events {
            if event.get("type").and_then(|v| v.as_str()) != Some("m.room.message") {
                continue;
            }
            let sender_id = event.get("sender").and_then(|v| v.as_str()).unwrap_or("");
            if sender_id == own_user_id {
                continue;
            }
            let Some(content) = event.get("content") else {
                continue;
            };
            let body = content.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let mimetype = content
                .pointer("/info/mimetype")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let (text, media) = match content.get("msgtype").and_then(|v| v.as_str()) {
                Some("m.text") => (Some(body.to_string()), Vec::new()),
                Some("m.image") => {
                    let Some(url) = content.get("url").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    // `body` doubles as the image caption.
                    (
                        Some(body.to_string()),
                        vec![Media::Image {
                            url: url.to_string(),
                            mime_type: mimetype,
                        }],
                    )
                }
                Some("m.file") => {
                    let Some(url) = content.get("url").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    (
                        None,
                        vec![Media::Document {
                            url: url.to_string(),
                            mime_type: mimetype,
                            name: Some(body.to_string()),
                        }],
                    )
                }
                _ => continue,
            };

            out.push(InboundMessage {
                channel: "matrix".into(),
                account_id: account_id.into(),
                peer: Peer {
                    kind: if is_direct {
                        PeerKind::Direct
                    } else {
                        PeerKind::Group
                    },
                    id: room_id.clone(),
                    name: None,
                    thread_id: None,
                },
                sender: Sender {
                    id: sender_id.into(),
                    display_name: None,
                    username: None,
                },
                message_id: event
                    .get("event_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                text,
                media,
                reply_to: None,
                timestamp: event
                    .get("origin_server_ts")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                is_mentioned: body.contains(own_user_id),
                ambient: false,
                guild_id: None,
                team_id: None,
            });
        }
    }

    out
}

/// Collect room ids listed in `account_data` `m.direct` events (the standard
/// user_id -> [room_ids] map), used to classify peers as Direct vs Group.
fn direct_room_ids(response: &Value) -> HashSet<&str> {
    let mut rooms = HashSet::new();
    let Some(events) = response
        .pointer("/account_data/events")
        .and_then(|v| v.as_array())
    else {
        return rooms;
    };

    for event in events {
        if event.get("type").and_then(|v| v.as_str()) != Some("m.direct") {
            continue;
        }
        if let Some(map) = event.get("content").and_then(|v| v.as_object()) {
            for room_list in map.values() {
                if let Some(list) = room_list.as_array() {
                    for room in list {
                        if let Some(id) = room.as_str() {
                            rooms.insert(id);
                        }
                    }
                }
            }
        }
    }

    rooms
}

async fn fetch_user_id(http: &reqwest::Client, cfg: &MatrixConfig) -> Result<String, ChannelError> {
    let url = format!("{}/_matrix/client/v3/account/whoami", cfg.homeserver);
    let response = http
        .get(&url)
        .bearer_auth(&cfg.access_token)
        .send()
        .await
        .map_err(|e| ChannelError::Runtime(format!("matrix whoami failed: {e}")))?;

    if !response.status().is_success() {
        return Err(ChannelError::Runtime(format!(
            "matrix whoami returned {}",
            response.status()
        )));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| ChannelError::Runtime(format!("matrix whoami decode failed: {e}")))?;
    body.get("user_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ChannelError::Runtime("matrix whoami response missing user_id".into()))
}

/// Outer long-poll loop. Each `/sync` request holds the connection for up to
/// 30 seconds server-side, so there is no client-side sleep on success; on
/// failure we back off 5 seconds like the other providers.
async fn sync_loop(
    http: &reqwest::Client,
    cfg: &MatrixConfig,
    inbound_tx: mpsc::Sender<InboundMessage>,
    running: Arc<AtomicBool>,
) {
    let Some(own_user_id) = cfg.user_id.clone() else {
        tracing::warn!("matrix sync loop started without a user id; stopping");
        return;
    };
    let mut since: Option<String> = None;

    while running.load(Ordering::SeqCst) {
        let mut url = format!("{}/_matrix/client/v3/sync?timeout=30000", cfg.homeserver);
        if let Some(since) = &since {
            url.push_str("&since=");
            url.push_str(&urlencoding_simple(since));
        }

        match http.get(&url).bearer_auth(&cfg.access_token).send().await {
            Ok(response) => match response.json::<Value>().await {
                Ok(body) => {
                    if let Some(next_batch) = body.get("next_batch").and_then(|v| v.as_str()) {
                        since = Some(next_batch.to_string());
                    }
                    for msg in parse_sync_response(&body, &own_user_id, &cfg.account_id) {
                        if inbound_tx.send(msg).await.is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to decode matrix sync response");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            },
            Err(err) => {
                tracing::warn!(error = %err, "matrix sync request failed");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}

/// Percent-encode the `/sync` `since` token's reserved characters.
fn urlencoding_simple(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for byte in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_response(events: Value, account_data: Value) -> Value {
        json!({
            "next_batch": "s2",
            "account_data": { "events": account_data },
            "rooms": {
                "join": {
                    "!room1:example.org": {
                        "timeline": { "events": events },
                    },
                },
            },
        })
    }

    fn message_event(msgtype: &str, body: &str, extra: Value) -> Value {
        let mut content = json!({ "msgtype": msgtype, "body": body });
        content
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "event_id": "$evt1",
            "origin_server_ts": 1700000000123u64,
            "content": content,
        })
    }

    #[test]
    fn parse_config_requires_homeserver_and_token() {
        let err = parse_config(json!({})).unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));

        let err = parse_config(json!({ "homeserver": "https://matrix.org" })).unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));

        let cfg = parse_config(json!({
            "homeserver": "https://matrix.org/",
            "accessToken": "tok",
        }))
        .unwrap();
        assert_eq!(cfg.homeserver, "https://matrix.org");
        assert_eq!(cfg.account_id, "default");
        assert_eq!(cfg.user_id, None);

        let cfg = parse_config(json!({
            "homeserver": "https://hs.example",
            "access_token": "tok",
            "user_id": "@bot:hs.example",
            "account_id": "work",
        }))
        .unwrap();
        assert_eq!(cfg.user_id.as_deref(), Some("@bot:hs.example"));
        assert_eq!(cfg.account_id, "work");
    }

    #[test]
    fn parses_joined_room_text_message() {
        let response = sync_response(
            json!([message_event("m.text", "hello legion", json!({}))]),
            json!([]),
        );
        let msgs = parse_sync_response(&response, "@bot:example.org", "acc1");
        assert_eq!(msgs.len(), 1);
        let msg = &msgs[0];
        assert_eq!(msg.channel, "matrix");
        assert_eq!(msg.account_id, "acc1");
        assert_eq!(msg.peer.kind, PeerKind::Group);
        assert_eq!(msg.peer.id, "!room1:example.org");
        assert_eq!(msg.sender.id, "@alice:example.org");
        assert_eq!(msg.message_id, "$evt1");
        assert_eq!(msg.text, Some("hello legion".into()));
        assert_eq!(msg.timestamp, "1700000000123");
        assert!(!msg.is_mentioned);
    }

    #[test]
    fn direct_rooms_from_account_data_become_direct() {
        let response = sync_response(
            json!([message_event("m.text", "hi", json!({}))]),
            json!([
                {
                    "type": "m.direct",
                    "content": { "@alice:example.org": ["!room1:example.org"] },
                },
            ]),
        );
        let msgs = parse_sync_response(&response, "@bot:example.org", "default");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].peer.kind, PeerKind::Direct);
    }

    #[test]
    fn skips_own_messages_and_other_event_types() {
        let mut own = message_event("m.text", "echo", json!({}));
        own["sender"] = json!("@bot:example.org");
        let response = sync_response(
            json!([
                own,
                { "type": "m.room.member", "sender": "@alice:example.org", "content": {} },
                message_event("m.notice", "notice", json!({})),
                message_event("m.text", "real", json!({})),
            ]),
            json!([]),
        );
        let msgs = parse_sync_response(&response, "@bot:example.org", "default");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, Some("real".into()));
    }

    #[test]
    fn parses_image_and_file_media() {
        let response = sync_response(
            json!([
                message_event(
                    "m.image",
                    "cat photo",
                    json!({
                        "url": "mxc://example.org/cat",
                        "info": { "mimetype": "image/png" },
                    })
                ),
                message_event(
                    "m.file",
                    "report.pdf",
                    json!({
                        "url": "mxc://example.org/report",
                        "info": { "mimetype": "application/pdf" },
                    })
                ),
            ]),
            json!([]),
        );
        let msgs = parse_sync_response(&response, "@bot:example.org", "default");
        assert_eq!(msgs.len(), 2);

        assert_eq!(msgs[0].text, Some("cat photo".into()));
        match &msgs[0].media[0] {
            Media::Image { url, mime_type } => {
                assert_eq!(url, "mxc://example.org/cat");
                assert_eq!(mime_type.as_deref(), Some("image/png"));
            }
            other => panic!("expected image, got {other:?}"),
        }

        assert_eq!(msgs[1].text, None);
        match &msgs[1].media[0] {
            Media::Document {
                url,
                mime_type,
                name,
            } => {
                assert_eq!(url, "mxc://example.org/report");
                assert_eq!(mime_type.as_deref(), Some("application/pdf"));
                assert_eq!(name.as_deref(), Some("report.pdf"));
            }
            other => panic!("expected document, got {other:?}"),
        }
    }

    #[test]
    fn detects_mention_of_own_user_id() {
        let response = sync_response(
            json!([message_event(
                "m.text",
                "hey @bot:example.org look",
                json!({})
            )]),
            json!([]),
        );
        let msgs = parse_sync_response(&response, "@bot:example.org", "default");
        assert!(msgs[0].is_mentioned);
    }

    #[test]
    fn empty_sync_returns_no_messages() {
        let response = json!({ "next_batch": "s9" });
        assert!(parse_sync_response(&response, "@bot:example.org", "default").is_empty());

        let response = sync_response(json!([]), json!([]));
        assert!(parse_sync_response(&response, "@bot:example.org", "default").is_empty());
    }

    #[test]
    fn since_token_is_percent_encoded() {
        assert_eq!(urlencoding_simple("s123_456.7~"), "s123_456.7~");
        assert_eq!(urlencoding_simple("a/b c+"), "a%2Fb%20c%2B");
    }
}
