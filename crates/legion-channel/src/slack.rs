use crate::util::{
    Lifecycle, StopPolicy, cfg_required, cfg_str_or, ensure_success, send_json, slack_envelope,
    ws_reconnect_loop,
};
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
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Built-in Slack channel provider using Socket Mode.
///
/// Requires a bot token (`xoxb-...`) for `chat.postMessage` and an app-level
/// token (`xapp-...`) with the `connections:write` scope to open Socket Mode
/// connections via `apps.connections.open`.
///
/// NOTE: the live socket path (connect / ack / reconnect) is covered only by
/// pure-function unit tests; it has not been exercised against the real Slack
/// API in this environment.
#[derive(Debug)]
pub struct SlackProvider {
    lifecycle: Lifecycle<SlackConfig>,
}

#[derive(Debug, Clone)]
struct SlackConfig {
    bot_token: String,
    app_token: String,
    base_url: String,
    account_id: String,
}

impl SlackProvider {
    pub fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
        }
    }
}

impl Default for SlackProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelProvider for SlackProvider {
    fn channel_id(&self) -> &str {
        "slack"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            text: true,
            media: vec!["image".into(), "document".into()],
            group: true,
            thread: true,
            reactions: true,
            typing: false,
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
                    "slack",
                    &running,
                    &mut state,
                    move || {
                        let http = open_http.clone();
                        let task_cfg = open_cfg.clone();
                        Box::pin(async move { open_socket_url(&http, &task_cfg).await })
                    },
                    move |_state: &mut (), url: String| {
                        let task_cfg = serve_cfg.clone();
                        let inbound_tx = inbound_tx.clone();
                        let running = serve_running.clone();
                        Box::pin(
                            async move { run_socket(&url, &task_cfg, &inbound_tx, &running).await },
                        )
                    },
                )
                .await;
            })
            .await;

        tracing::info!(channel = "slack", account = %account_id, "Slack channel started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChannelError> {
        self.lifecycle.stop(StopPolicy::Abort).await;
        tracing::info!(channel = "slack", "Slack channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        let cfg = self.lifecycle.config().await?;

        let mut payload = json!({
            "channel": message.peer.id,
            "text": message.text.unwrap_or_default(),
        });
        // Reply inside a thread when the message is a reply or the peer is a
        // thread-scoped conversation.
        if let Some(thread_ts) = message.reply_to.or(message.peer.thread_id) {
            payload["thread_ts"] = json!(thread_ts);
        }

        let url = format!("{}/chat.postMessage", cfg.base_url);
        let response = send_json(
            self.lifecycle.http.post(&url).bearer_auth(&cfg.bot_token),
            &payload,
        )
        .await?;
        let response = ensure_success(response, "slack chat.postMessage").await?;
        slack_envelope(response, "slack chat.postMessage").await?;

        Ok(())
    }
}

fn parse_config(config: Value) -> Result<SlackConfig, ChannelError> {
    let bot_token = cfg_required(
        &config,
        &["botToken", "bot_token"],
        "slack botToken is required",
    )?;

    let app_token = cfg_required(
        &config,
        &["appToken", "app_token"],
        "slack appToken is required",
    )?;

    let base_url = cfg_str_or(&config, &["baseUrl", "base_url"], "https://slack.com/api");

    let account_id = cfg_str_or(&config, &["accountId", "account_id"], "default");

    Ok(SlackConfig {
        bot_token,
        app_token,
        base_url,
        account_id,
    })
}

/// Parsed Socket Mode envelope.
#[derive(Debug, Clone, PartialEq)]
enum SocketEvent {
    /// Envelope that only needs an acknowledgement (non-message event).
    Ack(String),
    /// A chat message to route, plus the envelope id to acknowledge.
    Inbound(Box<InboundMessage>, Option<String>),
    /// Slack asked us to reconnect.
    Reconnect,
}

/// Parse a Socket Mode frame. Pure function for unit testing.
fn parse_socket_envelope(text: &str, account_id: &str) -> Option<SocketEvent> {
    let value: Value = serde_json::from_str(text).ok()?;
    let envelope_type = value.get("type")?.as_str()?;
    let envelope_id = value
        .get("envelope_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    match envelope_type {
        "disconnect" => Some(SocketEvent::Reconnect),
        "events_api" => {
            let event = value.get("payload")?.get("event")?;
            match parse_message_event(event, account_id) {
                Some(msg) => Some(SocketEvent::Inbound(Box::new(msg), envelope_id)),
                // Non-message events (reactions, joins, ...) must still be acked.
                None => envelope_id.map(SocketEvent::Ack),
            }
        }
        // Interactive payloads and slash commands are acked but not routed.
        "interactive" | "slash_commands" => envelope_id.map(SocketEvent::Ack),
        _ => None,
    }
}

/// Convert a Slack event object into an inbound message. Pure function.
fn parse_message_event(event: &Value, account_id: &str) -> Option<InboundMessage> {
    let event_type = event.get("type")?.as_str()?;
    if event_type != "message" && event_type != "app_mention" {
        return None;
    }
    // Skip message subtypes (edits, joins, bot messages, ...) entirely and any
    // message that carries a bot_id, to avoid loops with other bots.
    if event.get("subtype").is_some() || event.get("bot_id").is_some() {
        return None;
    }

    let channel_id = event.get("channel")?.as_str()?;
    let channel_type = event.get("channel_type").and_then(|v| v.as_str());
    let kind = if channel_type == Some("im") {
        PeerKind::Direct
    } else {
        PeerKind::Group
    };

    let thread_ts = event
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let ts = event
        .get("ts")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(InboundMessage {
        channel: "slack".into(),
        account_id: account_id.into(),
        peer: Peer {
            kind,
            id: channel_id.into(),
            name: None,
            thread_id: thread_ts,
        },
        sender: Sender {
            id: event
                .get("user")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .into(),
            display_name: None,
            username: None,
        },
        message_id: ts.clone(),
        text: event
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        media: vec![],
        reply_to: None,
        timestamp: ts,
        is_mentioned: event_type == "app_mention",
        ambient: false,
        guild_id: None,
        team_id: event
            .get("team")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

async fn open_socket_url(http: &reqwest::Client, cfg: &SlackConfig) -> Result<String, String> {
    let url = format!("{}/apps.connections.open", cfg.base_url);
    let response = http
        .post(&url)
        .bearer_auth(&cfg.app_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    if body.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        let error = body
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown_error");
        return Err(format!("apps.connections.open failed: {error}"));
    }

    body.get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "apps.connections.open response missing url".to_string())
}

/// One Socket Mode connection. Returns when the server asks us to reconnect,
/// the connection drops, or the inbound channel is closed.
async fn run_socket(
    url: &str,
    cfg: &SlackConfig,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    running: &Arc<AtomicBool>,
) -> Result<(), String> {
    let (ws, _) = connect_async(url).await.map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws.split();

    while let Some(frame) = read.next().await {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        let text = match frame {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => continue,
            Err(err) => return Err(err.to_string()),
        };

        match parse_socket_envelope(text.as_str(), &cfg.account_id) {
            Some(SocketEvent::Ack(envelope_id)) => {
                send_ack(&mut write, &envelope_id).await?;
            }
            Some(SocketEvent::Inbound(msg, envelope_id)) => {
                if let Some(envelope_id) = envelope_id {
                    send_ack(&mut write, &envelope_id).await?;
                }
                if inbound_tx.send(*msg).await.is_err() {
                    return Ok(());
                }
            }
            Some(SocketEvent::Reconnect) => return Ok(()),
            None => {}
        }
    }

    Ok(())
}

async fn send_ack<S>(write: &mut S, envelope_id: &str) -> Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
{
    let ack = json!({ "envelope_id": envelope_id }).to_string();
    write
        .send(Message::text(ack))
        .await
        .map_err(|_| "failed to send slack envelope ack".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(event: Value) -> String {
        json!({
            "envelope_id": "env-1",
            "type": "events_api",
            "payload": { "event": event },
        })
        .to_string()
    }

    #[test]
    fn parse_config_requires_tokens() {
        let err = parse_config(json!({})).unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));

        let err = parse_config(json!({ "botToken": "xoxb-1" })).unwrap_err();
        assert!(matches!(err, ChannelError::InvalidConfig(_)));

        let cfg = parse_config(json!({
            "botToken": "xoxb-1",
            "appToken": "xapp-1",
        }))
        .unwrap();
        assert_eq!(cfg.account_id, "default");
        assert_eq!(cfg.base_url, "https://slack.com/api");
    }

    #[test]
    fn parses_channel_message_envelope() {
        let frame = envelope(json!({
            "type": "message",
            "channel": "C123",
            "channel_type": "channel",
            "user": "U999",
            "text": "hello legion",
            "ts": "1700000000.000100",
            "team": "T42",
        }));

        match parse_socket_envelope(&frame, "acc1") {
            Some(SocketEvent::Inbound(msg, ack_id)) => {
                assert_eq!(ack_id, Some("env-1".into()));
                assert_eq!(msg.channel, "slack");
                assert_eq!(msg.account_id, "acc1");
                assert_eq!(msg.peer.kind, PeerKind::Group);
                assert_eq!(msg.peer.id, "C123");
                assert_eq!(msg.sender.id, "U999");
                assert_eq!(msg.text, Some("hello legion".into()));
                assert!(!msg.is_mentioned);
                assert_eq!(msg.team_id, Some("T42".into()));
            }
            other => panic!("expected inbound, got {other:?}"),
        }
    }

    #[test]
    fn parses_im_as_direct_and_thread() {
        let frame = envelope(json!({
            "type": "message",
            "channel": "D123",
            "channel_type": "im",
            "user": "U1",
            "text": "hi",
            "ts": "1700000001.0",
            "thread_ts": "1700000000.5",
        }));

        match parse_socket_envelope(&frame, "default") {
            Some(SocketEvent::Inbound(msg, _)) => {
                assert_eq!(msg.peer.kind, PeerKind::Direct);
                assert_eq!(msg.peer.thread_id, Some("1700000000.5".into()));
            }
            other => panic!("expected inbound, got {other:?}"),
        }
    }

    #[test]
    fn app_mention_sets_is_mentioned() {
        let frame = envelope(json!({
            "type": "app_mention",
            "channel": "C1",
            "user": "U1",
            "text": "<@B1> hey",
            "ts": "1.0",
        }));

        match parse_socket_envelope(&frame, "default") {
            Some(SocketEvent::Inbound(msg, _)) => assert!(msg.is_mentioned),
            other => panic!("expected inbound, got {other:?}"),
        }
    }

    #[test]
    fn skips_subtypes_and_bot_messages_but_acks() {
        let edited = envelope(json!({
            "type": "message",
            "subtype": "message_changed",
            "channel": "C1",
            "ts": "1.0",
        }));
        assert_eq!(
            parse_socket_envelope(&edited, "default"),
            Some(SocketEvent::Ack("env-1".into()))
        );

        let from_bot = envelope(json!({
            "type": "message",
            "bot_id": "B123",
            "channel": "C1",
            "user": "U1",
            "text": "bot echo",
            "ts": "1.0",
        }));
        assert_eq!(
            parse_socket_envelope(&from_bot, "default"),
            Some(SocketEvent::Ack("env-1".into()))
        );
    }

    #[test]
    fn disconnect_requests_reconnect() {
        let frame = json!({
            "type": "disconnect",
            "reason": "link_down",
        })
        .to_string();
        assert_eq!(
            parse_socket_envelope(&frame, "default"),
            Some(SocketEvent::Reconnect)
        );
    }

    #[test]
    fn non_event_envelopes_ack_or_ignore() {
        let slash = json!({
            "envelope_id": "env-9",
            "type": "slash_commands",
            "payload": {},
        })
        .to_string();
        assert_eq!(
            parse_socket_envelope(&slash, "default"),
            Some(SocketEvent::Ack("env-9".into()))
        );

        let hello = json!({ "type": "hello" }).to_string();
        assert_eq!(parse_socket_envelope(&hello, "default"), None);

        assert_eq!(parse_socket_envelope("not json", "default"), None);
    }
}
