use crate::util::{
    Lifecycle, StopPolicy, cfg_required, cfg_str_or, ensure_success, send_json, ws_reconnect_loop,
};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use legion_plugin_sdk::channel::{
    ChannelCapabilities, ChannelError, ChannelProvider, InboundMessage, Media, OutboundMessage,
    Peer, PeerKind, Sender,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Discord Gateway intent bits: GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES |
/// MESSAGE_CONTENT.
const INTENTS: u64 = 1 + 512 + 4096 + 32768;

/// Built-in Discord channel provider using the Gateway WebSocket.
///
/// Reconnects by re-opening the connection and re-sending IDENTIFY; RESUME is
/// intentionally not implemented, so events missed during a reconnect window
/// are lost (acceptable for an MVP channel).
///
/// NOTE: the live gateway path (connect / heartbeat / reconnect) is covered
/// only by pure-function unit tests; it has not been exercised against the
/// real Discord API in this environment.
#[derive(Debug)]
pub struct DiscordProvider {
    lifecycle: Lifecycle<DiscordConfig>,
    bot_user_id: Arc<Mutex<Option<String>>>,
}

#[derive(Debug, Clone)]
struct DiscordConfig {
    bot_token: String,
    base_url: String,
    account_id: String,
}

impl DiscordProvider {
    pub fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            bot_user_id: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for DiscordProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelProvider for DiscordProvider {
    fn channel_id(&self) -> &str {
        "discord"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            text: true,
            media: vec!["image".into(), "document".into()],
            group: true,
            thread: false,
            reactions: true,
            typing: true,
            buttons: false,
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
        let bot_user_id = self.bot_user_id.clone();
        let account_id = cfg.account_id.clone();
        let task_cfg = cfg.clone();

        self.lifecycle
            .begin(cfg, async move {
                let mut state = ();
                let open_http = http.clone();
                let open_cfg = task_cfg.clone();
                let serve_cfg = task_cfg.clone();
                let serve_running = running.clone();
                ws_reconnect_loop(
                    "discord",
                    &running,
                    &mut state,
                    move || {
                        let http = open_http.clone();
                        let task_cfg = open_cfg.clone();
                        Box::pin(async move { fetch_gateway_url(&http, &task_cfg).await })
                    },
                    move |_state: &mut (), url: String| {
                        let task_cfg = serve_cfg.clone();
                        let inbound_tx = inbound_tx.clone();
                        let running = serve_running.clone();
                        let bot_user_id = bot_user_id.clone();
                        Box::pin(async move {
                            run_gateway(&url, &task_cfg, &inbound_tx, &running, &bot_user_id).await
                        })
                    },
                )
                .await;
            })
            .await;

        tracing::info!(channel = "discord", account = %account_id, "Discord channel started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChannelError> {
        self.lifecycle.stop(StopPolicy::Abort).await;
        *self.bot_user_id.lock().await = None;
        tracing::info!(channel = "discord", "Discord channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        let cfg = self.lifecycle.config().await?;

        let url = format!("{}/channels/{}/messages", cfg.base_url, message.peer.id);
        let payload = json!({ "content": message.text.unwrap_or_default() });

        let response = send_json(
            self.lifecycle
                .http
                .post(&url)
                .header("Authorization", format!("Bot {}", cfg.bot_token)),
            &payload,
        )
        .await?;
        ensure_success(response, "discord send message").await?;

        Ok(())
    }
}

fn parse_config(config: Value) -> Result<DiscordConfig, ChannelError> {
    let bot_token = cfg_required(
        &config,
        &["botToken", "bot_token"],
        "discord botToken is required",
    )?;

    let base_url = cfg_str_or(
        &config,
        &["baseUrl", "base_url"],
        "https://discord.com/api/v10",
    );

    let account_id = cfg_str_or(&config, &["accountId", "account_id"], "default");

    Ok(DiscordConfig {
        bot_token,
        base_url,
        account_id,
    })
}

/// Convert a MESSAGE_CREATE dispatch payload into an inbound message.
/// Pure function for unit testing.
fn parse_message_create(
    d: &Value,
    bot_user_id: Option<&str>,
    account_id: &str,
) -> Option<InboundMessage> {
    let author = d.get("author")?;
    // Never route bot-authored messages (including our own echoes).
    if author.get("bot").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }

    let channel_id = d.get("channel_id")?.as_str()?;
    let guild_id = d
        .get("guild_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let kind = if guild_id.is_some() {
        PeerKind::Group
    } else {
        PeerKind::Direct
    };

    let is_mentioned = bot_user_id.is_some_and(|bot_id| {
        d.get("mentions")
            .and_then(|v| v.as_array())
            .is_some_and(|mentions| {
                mentions
                    .iter()
                    .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(bot_id))
            })
    });

    Some(InboundMessage {
        channel: "discord".into(),
        account_id: account_id.into(),
        peer: Peer {
            kind,
            id: channel_id.into(),
            name: None,
            thread_id: None,
        },
        sender: Sender {
            id: author
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .into(),
            display_name: author
                .get("global_name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            username: author
                .get("username")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        },
        message_id: d
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        text: d
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        media: extract_attachments(d),
        reply_to: d
            .pointer("/referenced_message/id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        timestamp: d
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        is_mentioned,
        ambient: false,
        guild_id,
        team_id: None,
    })
}

fn extract_attachments(d: &Value) -> Vec<Media> {
    let Some(attachments) = d.get("attachments").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    attachments
        .iter()
        .filter_map(|att| {
            let url = att.get("url")?.as_str()?.to_string();
            let mime_type = att
                .get("content_type")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if mime_type
                .as_deref()
                .is_some_and(|m| m.starts_with("image/"))
            {
                Some(Media::Image { url, mime_type })
            } else {
                Some(Media::Document {
                    url,
                    mime_type,
                    name: att
                        .get("filename")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
            }
        })
        .collect()
}

async fn fetch_gateway_url(http: &reqwest::Client, cfg: &DiscordConfig) -> Result<String, String> {
    let url = format!("{}/gateway/bot", cfg.base_url);
    let response = http
        .get(&url)
        .header("Authorization", format!("Bot {}", cfg.bot_token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("gateway/bot returned {}", response.status()));
    }

    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    body.get("url")
        .and_then(|v| v.as_str())
        .map(|base| format!("{base}/?v=10&encoding=json"))
        .ok_or_else(|| "gateway/bot response missing url".to_string())
}

/// One Gateway connection: HELLO → IDENTIFY → dispatch loop with heartbeats.
async fn run_gateway(
    ws_url: &str,
    cfg: &DiscordConfig,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    running: &Arc<AtomicBool>,
    bot_user_id: &Arc<Mutex<Option<String>>>,
) -> Result<(), String> {
    let (ws, _) = connect_async(ws_url).await.map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws.split();

    let mut heartbeat = tokio::time::interval(tokio::time::Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut identified = false;
    let mut last_seq: Option<u64> = None;

    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }

        tokio::select! {
            frame = read.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => continue,
                    Some(Err(err)) => return Err(err.to_string()),
                };

                let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let op = payload.get("op").and_then(|v| v.as_i64()).unwrap_or(-1);

                match op {
                    // HELLO: configure heartbeat interval and IDENTIFY.
                    10 => {
                        let interval_ms = payload
                            .pointer("/d/heartbeat_interval")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(30000);
                        heartbeat = tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
                        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        // Consume the immediate first tick so we don't send an
                        // early heartbeat.
                        heartbeat.tick().await;

                        let identify = json!({
                            "op": 2,
                            "d": {
                                "token": cfg.bot_token,
                                "intents": INTENTS,
                                "properties": {
                                    "os": "macos",
                                    "browser": "legion",
                                    "device": "legion",
                                },
                            },
                        });
                        write
                            .send(Message::text(identify.to_string()))
                            .await
                            .map_err(|e| e.to_string())?;
                        identified = true;
                    }
                    // Dispatch.
                    0 => {
                        if let Some(seq) = payload.get("s").and_then(|v| v.as_u64()) {
                            last_seq = Some(seq);
                        }
                        let event = payload.get("t").and_then(|v| v.as_str()).unwrap_or("");
                        match event {
                            "READY" => {
                                if let Some(user_id) =
                                    payload.pointer("/d/user/id").and_then(|v| v.as_str())
                                {
                                    *bot_user_id.lock().await = Some(user_id.to_string());
                                }
                            }
                            "MESSAGE_CREATE" => {
                                let bot_id = bot_user_id.lock().await.clone();
                                // Nested ifs instead of let-chains: the
                                // workspace MSRV is 1.86 (let-chains in `if`
                                // need 1.88).
                                if let Some(d) = payload.get("d") {
                                    if let Some(msg) =
                                        parse_message_create(d, bot_id.as_deref(), &cfg.account_id)
                                    {
                                        if inbound_tx.send(msg).await.is_err() {
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    // Reconnect / invalid session: outer loop re-identifies.
                    7 | 9 => return Ok(()),
                    // Heartbeat request from the server.
                    1 => {
                        send_heartbeat(&mut write, last_seq).await?;
                    }
                    _ => {}
                }
            }
            _ = heartbeat.tick(), if identified => {
                if send_heartbeat(&mut write, last_seq).await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

async fn send_heartbeat<S>(write: &mut S, last_seq: Option<u64>) -> Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
{
    let heartbeat = json!({ "op": 1, "d": last_seq }).to_string();
    write
        .send(Message::text(heartbeat))
        .await
        .map_err(|_| "failed to send discord heartbeat".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_requires_bot_token() {
        let err = parse_config(json!({})).unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));

        let cfg = parse_config(json!({ "botToken": "tok" })).unwrap();
        assert_eq!(cfg.account_id, "default");
        assert_eq!(cfg.base_url, "https://discord.com/api/v10");
    }

    #[test]
    fn parses_guild_message() {
        let d = json!({
            "id": "100",
            "channel_id": "200",
            "guild_id": "300",
            "content": "hello legion",
            "timestamp": "2026-01-01T00:00:00Z",
            "author": { "id": "42", "username": "alice", "global_name": "Alice" },
            "mentions": [],
        });

        let msg = parse_message_create(&d, Some("999"), "acc1").unwrap();
        assert_eq!(msg.channel, "discord");
        assert_eq!(msg.account_id, "acc1");
        assert_eq!(msg.peer.kind, PeerKind::Group);
        assert_eq!(msg.peer.id, "200");
        assert_eq!(msg.sender.id, "42");
        assert_eq!(msg.sender.username, Some("alice".into()));
        assert_eq!(msg.text, Some("hello legion".into()));
        assert!(!msg.is_mentioned);
        assert_eq!(msg.guild_id, Some("300".into()));
    }

    #[test]
    fn parses_dm_as_direct() {
        let d = json!({
            "id": "1",
            "channel_id": "55",
            "content": "hi",
            "author": { "id": "7", "username": "bob" },
            "mentions": [],
        });

        let msg = parse_message_create(&d, None, "default").unwrap();
        assert_eq!(msg.peer.kind, PeerKind::Direct);
        assert_eq!(msg.guild_id, None);
    }

    #[test]
    fn skips_bot_authored_messages() {
        let d = json!({
            "id": "1",
            "channel_id": "2",
            "content": "echo",
            "author": { "id": "9", "username": "otherbot", "bot": true },
        });
        assert!(parse_message_create(&d, Some("3"), "default").is_none());
    }

    #[test]
    fn detects_mention_of_bot_user() {
        let d = json!({
            "id": "1",
            "channel_id": "2",
            "guild_id": "3",
            "content": "<@999> hey",
            "author": { "id": "7", "username": "alice" },
            "mentions": [{ "id": "999", "username": "legion" }],
        });

        let msg = parse_message_create(&d, Some("999"), "default").unwrap();
        assert!(msg.is_mentioned);

        let msg = parse_message_create(&d, Some("123"), "default").unwrap();
        assert!(!msg.is_mentioned);
    }

    #[test]
    fn extracts_image_and_document_attachments() {
        let d = json!({
            "id": "1",
            "channel_id": "2",
            "author": { "id": "7", "username": "alice" },
            "attachments": [
                {
                    "url": "https://cdn.discord.com/a.png",
                    "content_type": "image/png",
                    "filename": "a.png",
                },
                {
                    "url": "https://cdn.discord.com/b.pdf",
                    "content_type": "application/pdf",
                    "filename": "b.pdf",
                }
            ],
        });

        let msg = parse_message_create(&d, None, "default").unwrap();
        assert_eq!(msg.media.len(), 2);
        match &msg.media[0] {
            Media::Image { url, mime_type } => {
                assert_eq!(url, "https://cdn.discord.com/a.png");
                assert_eq!(mime_type.as_deref(), Some("image/png"));
            }
            other => panic!("expected image, got {other:?}"),
        }
        match &msg.media[1] {
            Media::Document { url, name, .. } => {
                assert_eq!(url, "https://cdn.discord.com/b.pdf");
                assert_eq!(name.as_deref(), Some("b.pdf"));
            }
            other => panic!("expected document, got {other:?}"),
        }
    }
}
