use async_trait::async_trait;
use legion_plugin_sdk::channel::{
    ChannelCapabilities, ChannelError, ChannelProvider, InboundMessage, Media, OutboundMessage,
    Peer, PeerKind, Sender,
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, mpsc};

/// Built-in Telegram channel provider using the Bot API.
///
/// For the MVP this uses `reqwest` long polling against
/// `https://api.telegram.org/bot<token>/getUpdates`.
#[derive(Debug)]
pub struct TelegramProvider {
    http: reqwest::Client,
    config: Mutex<Option<TelegramConfig>>,
    running: Arc<AtomicBool>,
    poll_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
struct TelegramConfig {
    token: String,
    base_url: String,
    account_id: String,
    /// Bot username (without the leading `@`) used for group mention
    /// detection. Optional: without it any mention entity counts.
    bot_username: Option<String>,
}

impl TelegramProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            config: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            poll_handle: Mutex::new(None),
        }
    }

    /// Build a provider using a custom HTTP client (useful in tests).
    pub fn with_http(http: reqwest::Client) -> Self {
        Self {
            http,
            config: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            poll_handle: Mutex::new(None),
        }
    }
}

impl Default for TelegramProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelProvider for TelegramProvider {
    fn channel_id(&self) -> &str {
        "telegram"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            text: true,
            media: vec![
                "image".into(),
                "audio".into(),
                "video".into(),
                "document".into(),
            ],
            group: true,
            thread: false,
            reactions: true,
            typing: true,
        }
    }

    async fn start(
        &self,
        config: serde_json::Value,
        inbound_tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), ChannelError> {
        let cfg = parse_config(config)?;
        *self.config.lock().await = Some(cfg.clone());
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let http = self.http.clone();
        let token = cfg.token.clone();
        let base_url = cfg.base_url.clone();
        let account_id = cfg.account_id.clone();
        let bot_username = cfg.bot_username.clone();

        let handle = tokio::spawn(async move {
            poll_updates(
                &http,
                &base_url,
                &token,
                &account_id,
                bot_username.as_deref(),
                inbound_tx,
                running,
            )
            .await;
        });

        *self.poll_handle.lock().await = Some(handle);
        tracing::info!(channel = "telegram", account = %cfg.account_id, "Telegram channel started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChannelError> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.poll_handle.lock().await.take() {
            let _ = handle.await;
        }
        *self.config.lock().await = None;
        tracing::info!(channel = "telegram", "Telegram channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        let cfg = self
            .config
            .lock()
            .await
            .clone()
            .ok_or(ChannelError::NotStarted)?;

        let chat_id = message.peer.id.parse::<i64>().map_err(|_| {
            ChannelError::SendFailed(format!("invalid telegram chat id: {}", message.peer.id))
        })?;

        let url = format!("{}/bot{}/sendMessage", cfg.base_url, cfg.token);
        let payload = SendMessagePayload {
            chat_id,
            text: message.text.unwrap_or_default(),
            reply_parameters: message
                .reply_to
                .and_then(|message_id| message_id.parse::<i64>().ok())
                .map(|message_id| ReplyParameters { message_id }),
        };

        let response = self
            .http
            .post(&url)
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
                "telegram sendMessage failed: {status} {body}"
            )));
        }

        Ok(())
    }

    async fn send_typing(&self, peer: &Peer) -> Result<(), ChannelError> {
        let cfg = self
            .config
            .lock()
            .await
            .clone()
            .ok_or(ChannelError::NotStarted)?;

        let chat_id = peer.id.parse::<i64>().map_err(|_| {
            ChannelError::SendFailed(format!("invalid telegram chat id: {}", peer.id))
        })?;

        let url = format!("{}/bot{}/sendChatAction", cfg.base_url, cfg.token);
        let payload = SendChatActionPayload {
            chat_id,
            action: "typing",
        };

        let response = self
            .http
            .post(&url)
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
                "telegram sendChatAction failed: {status} {body}"
            )));
        }

        Ok(())
    }

    async fn add_reaction(
        &self,
        peer: &Peer,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), ChannelError> {
        let cfg = self
            .config
            .lock()
            .await
            .clone()
            .ok_or(ChannelError::NotStarted)?;

        let chat_id = peer.id.parse::<i64>().map_err(|_| {
            ChannelError::SendFailed(format!("invalid telegram chat id: {}", peer.id))
        })?;
        let message_id = message_id.parse::<i64>().map_err(|_| {
            ChannelError::SendFailed(format!("invalid telegram message id: {message_id}"))
        })?;

        let url = format!("{}/bot{}/setMessageReaction", cfg.base_url, cfg.token);
        let payload = SetMessageReactionPayload {
            chat_id,
            message_id,
            reaction: vec![ReactionType {
                r#type: "emoji",
                emoji: emoji.to_string(),
            }],
        };

        let response = self
            .http
            .post(&url)
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
                "telegram setMessageReaction failed: {status} {body}"
            )));
        }

        Ok(())
    }
}

fn parse_config(config: serde_json::Value) -> Result<TelegramConfig, ChannelError> {
    let token = config
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ChannelError::InvalidConfig("telegram token is required".into()))?
        .to_string();

    let base_url = config
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("https://api.telegram.org")
        .to_string();

    let account_id = config
        .get("account_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    let bot_username = config
        .get("botUsername")
        .or_else(|| config.get("bot_username"))
        .and_then(|v| v.as_str())
        .map(|name| name.trim_start_matches('@').to_string())
        .filter(|name| !name.is_empty());

    Ok(TelegramConfig {
        token,
        base_url,
        account_id,
        bot_username,
    })
}

async fn poll_updates(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    account_id: &str,
    bot_username: Option<&str>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    running: Arc<AtomicBool>,
) {
    let mut offset: Option<i64> = None;
    let url = format!("{}/bot{}/getUpdates", base_url, token);

    while running.load(Ordering::SeqCst) {
        let mut request = http.get(&url).query(&[("limit", "100"), ("timeout", "30")]);
        if let Some(off) = offset {
            request = request.query(&[("offset", off.to_string())]);
        }

        match request.send().await {
            Ok(response) => {
                if !response.status().is_success() {
                    tracing::warn!(
                        status = %response.status(),
                        "telegram getUpdates returned error"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }

                match response.json::<ApiResponse<Vec<Update>>>().await {
                    Ok(updates) => {
                        for update in updates.result.unwrap_or_default() {
                            if let Some(message) = update.message {
                                if let Some(inbound) = convert_message(
                                    &message,
                                    account_id,
                                    update.update_id,
                                    bot_username,
                                ) {
                                    if inbound_tx.send(inbound).await.is_err() {
                                        tracing::debug!("inbound channel closed, stopping poll");
                                        return;
                                    }
                                }
                                offset = Some(update.update_id + 1);
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to parse telegram updates");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "telegram getUpdates request failed");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}

fn convert_message(
    message: &TelegramMessage,
    account_id: &str,
    _update_id: i64,
    bot_username: Option<&str>,
) -> Option<InboundMessage> {
    let text = message.text.as_deref().unwrap_or("");
    let is_start = text.trim() == "/start";

    let peer_kind = match message.chat.r#type.as_str() {
        "private" => PeerKind::Direct,
        "group" | "supergroup" => PeerKind::Group,
        _ => PeerKind::Direct,
    };

    let peer_name = message.chat.title.clone().or_else(|| {
        if peer_kind == PeerKind::Direct {
            message.from.as_ref().and_then(|u| u.first_name.clone())
        } else {
            None
        }
    });

    let display_name = message
        .from
        .as_ref()
        .and_then(|u| u.first_name.clone())
        .or_else(|| message.from.as_ref().and_then(|u| u.username.clone()));

    let username = message.from.as_ref().and_then(|u| u.username.clone());

    let final_text = if is_start {
        Some("/start".into())
    } else {
        message.text.clone()
    };

    let sender_id = message
        .from
        .as_ref()
        .map(|u| u.id.to_string())
        .unwrap_or_else(|| "unknown".into());

    let media = extract_media(message);

    Some(InboundMessage {
        channel: "telegram".into(),
        account_id: account_id.into(),
        peer: Peer {
            kind: peer_kind,
            id: message.chat.id.to_string(),
            name: peer_name,
            thread_id: None,
        },
        sender: Sender {
            id: sender_id,
            display_name,
            username,
        },
        message_id: format!("{}", message.message_id),
        text: final_text,
        media,
        reply_to: message
            .reply_to_message
            .as_ref()
            .map(|m| m.message_id.to_string()),
        timestamp: timestamp_from_unix(message.date),
        is_mentioned: compute_is_mentioned(message, bot_username),
        ambient: false,
        guild_id: None,
        team_id: None,
    })
}

/// Decide whether the bot was mentioned in a group message.
///
/// With a configured `bot_username`, a `mention` entity must cover exactly
/// `@<bot_username>` (case-insensitive) or a `text_mention` entity's user
/// username must match. Without it, fall back to Lark-style detection: any
/// `mention`/`text_mention` entity counts. A bare `reply_to_message` never
/// counts as a mention.
fn compute_is_mentioned(message: &TelegramMessage, bot_username: Option<&str>) -> bool {
    let entities = message.entities.as_deref().unwrap_or(&[]);
    match bot_username {
        Some(bot) => {
            let target = format!("@{bot}");
            entities.iter().any(|entity| match entity.r#type.as_str() {
                "mention" => message
                    .text
                    .as_deref()
                    .and_then(|text| entity_text(text, entity))
                    .is_some_and(|mention| mention.eq_ignore_ascii_case(&target)),
                "text_mention" => entity
                    .user
                    .as_ref()
                    .and_then(|user| user.username.as_deref())
                    .is_some_and(|username| username.eq_ignore_ascii_case(bot)),
                _ => false,
            })
        }
        None => entities
            .iter()
            .any(|entity| matches!(entity.r#type.as_str(), "mention" | "text_mention")),
    }
}

/// Extract the text covered by a message entity. Telegram entity offsets are
/// in UTF-16 code units, so slice in UTF-16 space.
fn entity_text(text: &str, entity: &MessageEntity) -> Option<String> {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let start = entity.offset as usize;
    let end = start.checked_add(entity.length as usize)?;
    String::from_utf16(utf16.get(start..end)?).ok()
}

fn extract_media(message: &TelegramMessage) -> Vec<Media> {
    let mut media = Vec::new();
    if let Some(photo) = message.photo.as_ref().and_then(|p| p.last()) {
        media.push(Media::Image {
            url: photo.file_id.clone(),
            mime_type: None,
        });
    }
    if let Some(voice) = &message.voice {
        media.push(Media::Audio {
            url: voice.file_id.clone(),
            mime_type: Some("audio/ogg".into()),
        });
    }
    if let Some(video) = &message.video {
        media.push(Media::Video {
            url: video.file_id.clone(),
            mime_type: None,
        });
    }
    if let Some(doc) = &message.document {
        media.push(Media::Document {
            url: doc.file_id.clone(),
            mime_type: doc.mime_type.clone(),
            name: doc.file_name.clone(),
        });
    }
    media
}

fn timestamp_from_unix(secs: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(secs);
    format!("{}Z", dt.duration_since(UNIX_EPOCH).unwrap().as_secs())
}

// Telegram API types

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub from: Option<TelegramUser>,
    pub chat: TelegramChat,
    pub date: u64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub entities: Option<Vec<MessageEntity>>,
    #[serde(default)]
    pub photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub video: Option<Video>,
    #[serde(default)]
    pub document: Option<Document>,
    #[serde(default)]
    pub reply_to_message: Option<Box<TelegramMessage>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageEntity {
    #[serde(rename = "type")]
    pub r#type: String,
    pub offset: u32,
    pub length: u32,
    #[serde(default)]
    pub user: Option<TelegramUser>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramUser {
    pub id: i64,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Voice {
    pub file_id: String,
    pub file_unique_id: String,
    pub duration: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Video {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: i32,
    pub height: i32,
    pub duration: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Document {
    pub file_id: String,
    pub file_unique_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SendMessagePayload {
    chat_id: i64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_parameters: Option<ReplyParameters>,
}

#[derive(Debug, Clone, Serialize)]
struct ReplyParameters {
    message_id: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SendChatActionPayload {
    chat_id: i64,
    action: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct SetMessageReactionPayload {
    chat_id: i64,
    message_id: i64,
    reaction: Vec<ReactionType>,
}

#[derive(Debug, Clone, Serialize)]
struct ReactionType {
    r#type: &'static str,
    emoji: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_parse_dm_update() {
        let update_json = json!({
            "ok": true,
            "result": [
                {
                    "update_id": 123456789,
                    "message": {
                        "message_id": 42,
                        "from": { "id": 111, "first_name": "Alice", "username": "alice" },
                        "chat": { "id": 111, "type": "private" },
                        "date": 1620000000,
                        "text": "hello bot"
                    }
                }
            ]
        });

        let parsed: ApiResponse<Vec<Update>> = serde_json::from_value(update_json).unwrap();
        let updates = parsed.result.unwrap();
        assert_eq!(updates.len(), 1);

        let msg = convert_message(
            updates[0].message.as_ref().unwrap(),
            "default",
            123456789,
            None,
        )
        .unwrap();
        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.peer.kind, PeerKind::Direct);
        assert_eq!(msg.peer.id, "111");
        assert_eq!(msg.sender.id, "111");
        assert_eq!(msg.sender.display_name, Some("Alice".into()));
        assert_eq!(msg.sender.username, Some("alice".into()));
        assert_eq!(msg.text, Some("hello bot".into()));
    }

    #[test]
    fn should_parse_group_update() {
        let update_json = json!({
            "ok": true,
            "result": [
                {
                    "update_id": 987654321,
                    "message": {
                        "message_id": 7,
                        "from": { "id": 222, "first_name": "Bob", "username": "bob" },
                        "chat": { "id": -100123456, "type": "supergroup", "title": "Legion Chat" },
                        "date": 1620000001,
                        "text": "hi all"
                    }
                }
            ]
        });

        let parsed: ApiResponse<Vec<Update>> = serde_json::from_value(update_json).unwrap();
        let msg = convert_message(
            parsed.result.unwrap()[0].message.as_ref().unwrap(),
            "default",
            987654321,
            None,
        )
        .unwrap();

        assert_eq!(msg.peer.kind, PeerKind::Group);
        assert_eq!(msg.peer.id, "-100123456");
        assert_eq!(msg.peer.name, Some("Legion Chat".into()));
        assert_eq!(msg.sender.id, "222");
    }

    #[test]
    fn should_recognize_start_command() {
        let update_json = json!({
            "ok": true,
            "result": [
                {
                    "update_id": 1,
                    "message": {
                        "message_id": 1,
                        "from": { "id": 333, "first_name": "Carol" },
                        "chat": { "id": 333, "type": "private" },
                        "date": 1620000002,
                        "text": "/start"
                    }
                }
            ]
        });

        let parsed: ApiResponse<Vec<Update>> = serde_json::from_value(update_json).unwrap();
        let msg = convert_message(
            parsed.result.unwrap()[0].message.as_ref().unwrap(),
            "default",
            1,
            None,
        )
        .unwrap();

        assert_eq!(msg.text, Some("/start".into()));
    }

    #[test]
    fn should_parse_media_update() {
        let update_json = json!({
            "ok": true,
            "result": [
                {
                    "update_id": 2,
                    "message": {
                        "message_id": 3,
                        "from": { "id": 444, "first_name": "Dave" },
                        "chat": { "id": 444, "type": "private" },
                        "date": 1620000003,
                        "caption": "look at this",
                        "photo": [
                            { "file_id": "small", "file_unique_id": "u1", "width": 100, "height": 100 },
                            { "file_id": "large", "file_unique_id": "u2", "width": 800, "height": 600 }
                        ]
                    }
                }
            ]
        });

        let parsed: ApiResponse<Vec<Update>> = serde_json::from_value(update_json).unwrap();
        let msg = convert_message(
            parsed.result.unwrap()[0].message.as_ref().unwrap(),
            "default",
            2,
            None,
        )
        .unwrap();

        assert_eq!(msg.media.len(), 1);
        match &msg.media[0] {
            Media::Image { url, .. } => assert_eq!(url, "large"),
            _ => panic!("expected image media"),
        }
    }

    #[test]
    fn parse_config_reads_optional_bot_username() {
        let cfg = parse_config(json!({ "token": "t" })).unwrap();
        assert_eq!(cfg.bot_username, None);

        let cfg = parse_config(json!({ "token": "t", "botUsername": "@legion_bot" })).unwrap();
        assert_eq!(cfg.bot_username.as_deref(), Some("legion_bot"));

        let cfg = parse_config(json!({ "token": "t", "bot_username": "legion_bot" })).unwrap();
        assert_eq!(cfg.bot_username.as_deref(), Some("legion_bot"));
    }

    /// Build a supergroup `TelegramMessage` with optional message entities.
    fn group_message(text: &str, entities: Option<serde_json::Value>) -> TelegramMessage {
        let mut value = json!({
            "message_id": 7,
            "from": { "id": 222, "first_name": "Bob", "username": "bob" },
            "chat": { "id": -100123456, "type": "supergroup", "title": "Legion Chat" },
            "date": 1620000001,
            "text": text
        });
        if let Some(entities) = entities {
            value["entities"] = entities;
        }
        serde_json::from_value(value).expect("group message should deserialize")
    }

    #[test]
    fn group_message_with_bot_mention_entity_is_flagged() {
        // "@legion_bot" is 11 ASCII chars -> offset 0, length 11.
        let msg = group_message(
            "@legion_bot hi",
            Some(json!([{ "type": "mention", "offset": 0, "length": 11 }])),
        );
        let inbound = convert_message(&msg, "default", 1, Some("legion_bot")).unwrap();
        assert!(inbound.is_mentioned);
    }

    #[test]
    fn group_message_mention_match_is_case_insensitive() {
        let msg = group_message(
            "@LEGION_BOT hi",
            Some(json!([{ "type": "mention", "offset": 0, "length": 11 }])),
        );
        let inbound = convert_message(&msg, "default", 1, Some("legion_bot")).unwrap();
        assert!(inbound.is_mentioned);
    }

    #[test]
    fn group_message_without_entities_is_not_flagged() {
        let msg = group_message("hi all", None);
        let inbound = convert_message(&msg, "default", 1, Some("legion_bot")).unwrap();
        assert!(!inbound.is_mentioned);
    }

    #[test]
    fn mention_of_different_user_is_not_flagged() {
        // "@someone_else" is 13 ASCII chars -> offset 0, length 13.
        let msg = group_message(
            "@someone_else hi",
            Some(json!([{ "type": "mention", "offset": 0, "length": 13 }])),
        );
        let inbound = convert_message(&msg, "default", 1, Some("legion_bot")).unwrap();
        assert!(!inbound.is_mentioned);
    }

    #[test]
    fn text_mention_flags_only_matching_username() {
        let msg = group_message(
            "hey bot",
            Some(json!([{
                "type": "text_mention",
                "offset": 4,
                "length": 3,
                "user": { "id": 999, "username": "legion_bot" }
            }])),
        );
        let inbound = convert_message(&msg, "default", 1, Some("legion_bot")).unwrap();
        assert!(inbound.is_mentioned);

        let inbound = convert_message(&msg, "default", 1, Some("other_bot")).unwrap();
        assert!(!inbound.is_mentioned);
    }

    #[test]
    fn any_mention_counts_when_bot_username_unknown() {
        let msg = group_message(
            "@someone_else hi",
            Some(json!([{ "type": "mention", "offset": 0, "length": 13 }])),
        );
        let inbound = convert_message(&msg, "default", 1, None).unwrap();
        assert!(inbound.is_mentioned);

        // No entities at all: not mentioned even in fallback mode.
        let msg = group_message("hi all", None);
        let inbound = convert_message(&msg, "default", 1, None).unwrap();
        assert!(!inbound.is_mentioned);
    }

    #[test]
    fn reply_to_message_alone_is_not_a_mention() {
        let mut value = json!({
            "message_id": 7,
            "from": { "id": 222, "first_name": "Bob", "username": "bob" },
            "chat": { "id": -100123456, "type": "supergroup", "title": "Legion Chat" },
            "date": 1620000001,
            "text": "ok",
            "reply_to_message": {
                "message_id": 6,
                "chat": { "id": -100123456, "type": "supergroup" },
                "date": 1620000000
            }
        });
        let msg: TelegramMessage =
            serde_json::from_value(value.clone()).expect("reply message should deserialize");
        let inbound = convert_message(&msg, "default", 1, Some("legion_bot")).unwrap();
        assert!(!inbound.is_mentioned);
        assert_eq!(inbound.reply_to, Some("6".into()));

        // In fallback mode a bare reply is not a mention either.
        value["entities"] = json!([]);
        let msg: TelegramMessage = serde_json::from_value(value).unwrap();
        let inbound = convert_message(&msg, "default", 1, None).unwrap();
        assert!(!inbound.is_mentioned);
    }

    #[test]
    fn dm_conversion_is_unaffected_by_mention_logic() {
        let update_json = json!({
            "ok": true,
            "result": [
                {
                    "update_id": 123456789,
                    "message": {
                        "message_id": 42,
                        "from": { "id": 111, "first_name": "Alice", "username": "alice" },
                        "chat": { "id": 111, "type": "private" },
                        "date": 1620000000,
                        "text": "hello bot"
                    }
                }
            ]
        });

        let parsed: ApiResponse<Vec<Update>> = serde_json::from_value(update_json).unwrap();
        let msg = convert_message(
            parsed.result.unwrap()[0].message.as_ref().unwrap(),
            "default",
            123456789,
            Some("legion_bot"),
        )
        .unwrap();
        assert_eq!(msg.peer.kind, PeerKind::Direct);
        assert_eq!(msg.text, Some("hello bot".into()));
        assert!(!msg.is_mentioned);
    }

    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn started_provider(base_url: String) -> TelegramProvider {
        TelegramProvider {
            http: reqwest::Client::new(),
            config: Mutex::new(Some(TelegramConfig {
                token: "t".into(),
                base_url,
                account_id: "default".into(),
                bot_username: None,
            })),
            running: Arc::new(AtomicBool::new(false)),
            poll_handle: Mutex::new(None),
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
    async fn send_typing_posts_send_chat_action() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bott/sendChatAction"))
            .and(body_json(json!({ "chat_id": 42, "action": "typing" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = started_provider(server.uri());
        provider
            .send_typing(&dm_peer("42"))
            .await
            .expect("send_typing should succeed");
    }

    #[tokio::test]
    async fn send_typing_returns_send_failed_on_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bott/sendChatAction"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
            .mount(&server)
            .await;

        let provider = started_provider(server.uri());
        let err = provider
            .send_typing(&dm_peer("42"))
            .await
            .expect_err("send_typing should fail on 500");
        match err {
            ChannelError::SendFailed(msg) => {
                assert!(msg.contains("sendChatAction"), "unexpected error: {msg}");
                assert!(msg.contains("500"), "unexpected error: {msg}");
            }
            other => panic!("expected SendFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_typing_fails_when_not_started() {
        let provider = TelegramProvider::new();
        let err = provider
            .send_typing(&dm_peer("42"))
            .await
            .expect_err("send_typing should fail when not started");
        assert_eq!(err, ChannelError::NotStarted);
    }

    #[tokio::test]
    async fn send_typing_rejects_non_numeric_chat_id() {
        let server = MockServer::start().await;
        let provider = started_provider(server.uri());
        let err = provider
            .send_typing(&dm_peer("not-a-number"))
            .await
            .expect_err("send_typing should fail on a non-numeric chat id");
        assert!(matches!(err, ChannelError::SendFailed(_)));
    }

    #[tokio::test]
    async fn add_reaction_posts_set_message_reaction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bott/setMessageReaction"))
            .and(body_json(json!({
                "chat_id": 42,
                "message_id": 7,
                "reaction": [{ "type": "emoji", "emoji": "👀" }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = started_provider(server.uri());
        provider
            .add_reaction(&dm_peer("42"), "7", "👀")
            .await
            .expect("add_reaction should succeed");
    }

    #[tokio::test]
    async fn add_reaction_returns_send_failed_on_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bott/setMessageReaction"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
            .mount(&server)
            .await;

        let provider = started_provider(server.uri());
        let err = provider
            .add_reaction(&dm_peer("42"), "7", "👀")
            .await
            .expect_err("add_reaction should fail on 500");
        match err {
            ChannelError::SendFailed(msg) => {
                assert!(
                    msg.contains("setMessageReaction"),
                    "unexpected error: {msg}"
                );
                assert!(msg.contains("500"), "unexpected error: {msg}");
            }
            other => panic!("expected SendFailed, got {other:?}"),
        }
    }
}
