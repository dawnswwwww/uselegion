use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A peer on a channel: direct message, group, or thread.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Peer {
    pub kind: PeerKind,
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PeerKind {
    Direct,
    Group,
    Thread,
}

/// Sender identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sender {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

/// A media attachment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Media {
    Image {
        url: String,
        mime_type: Option<String>,
    },
    Audio {
        url: String,
        mime_type: Option<String>,
    },
    Video {
        url: String,
        mime_type: Option<String>,
    },
    Document {
        url: String,
        mime_type: Option<String>,
        name: Option<String>,
    },
}

/// A message received from a channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboundMessage {
    pub channel: String,
    pub account_id: String,
    pub peer: Peer,
    pub sender: Sender,
    pub message_id: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub media: Vec<Media>,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub timestamp: String,
    #[serde(default)]
    pub is_mentioned: bool,
    #[serde(default)]
    pub ambient: bool,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
}

impl InboundMessage {
    pub fn direct(
        channel: impl Into<String>,
        account_id: impl Into<String>,
        sender_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let sender_id = sender_id.into();
        Self {
            channel: channel.into(),
            account_id: account_id.into(),
            peer: Peer {
                kind: PeerKind::Direct,
                id: sender_id.clone(),
                name: None,
                thread_id: None,
            },
            sender: Sender {
                id: sender_id,
                display_name: None,
                username: None,
            },
            message_id: uuid(),
            text: Some(text.into()),
            media: vec![],
            reply_to: None,
            timestamp: now_iso(),
            is_mentioned: false,
            ambient: false,
            guild_id: None,
            team_id: None,
        }
    }
}

/// A message to be sent out on a channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutboundMessage {
    pub channel: String,
    pub account_id: String,
    pub peer: Peer,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub media: Vec<Media>,
    #[serde(default)]
    pub reply_to: Option<String>,
}

/// Capabilities advertised by a channel provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChannelCapabilities {
    pub text: bool,
    #[serde(default)]
    pub media: Vec<String>,
    pub group: bool,
    pub thread: bool,
    pub reactions: bool,
    pub typing: bool,
    /// Whether the provider can send interactive approval cards with
    /// 批准/拒绝 buttons (see [`ChannelProvider::send_approval_card`]).
    #[serde(default)]
    pub buttons: bool,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ChannelError {
    #[error("channel not started")]
    NotStarted,
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("runtime error: {0}")]
    Runtime(String),
}

/// Trait implemented by channel plugins.
#[async_trait]
pub trait ChannelProvider: Send + Sync {
    fn channel_id(&self) -> &str;

    fn capabilities(&self) -> ChannelCapabilities;

    /// Start the channel.
    ///
    /// The provider should use `inbound_tx` to deliver messages received from the
    /// external chat service to the Gateway router.
    async fn start(
        &self,
        config: serde_json::Value,
        inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    ) -> Result<(), ChannelError>;

    async fn stop(&self) -> Result<(), ChannelError>;

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError>;

    /// Send a typing indicator to the peer. Default: no-op.
    async fn send_typing(&self, _peer: &Peer) -> Result<(), ChannelError> {
        Ok(())
    }

    /// Add a reaction to a message. Default: no-op.
    async fn add_reaction(
        &self,
        _peer: &Peer,
        _message_id: &str,
        _emoji: &str,
    ) -> Result<(), ChannelError> {
        Ok(())
    }

    /// Remove a previously added reaction. Default: no-op.
    async fn remove_reaction(
        &self,
        _peer: &Peer,
        _message_id: &str,
        _emoji: &str,
    ) -> Result<(), ChannelError> {
        Ok(())
    }

    /// Send an interactive tool-approval card with 批准/拒绝 buttons.
    ///
    /// Returns `Ok(true)` when the card was sent, `Ok(false)` when the
    /// provider does not support cards and the caller should fall back to a
    /// plain text message. Default: unsupported.
    async fn send_approval_card(
        &self,
        _peer: &Peer,
        _tool: &str,
        _prompt_id: &str,
    ) -> Result<bool, ChannelError> {
        Ok(false)
    }
}

fn uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("msg-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{}Z", secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_create_direct_inbound_message() {
        let msg = InboundMessage::direct("telegram", "default", "u123", "hello");

        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.account_id, "default");
        assert_eq!(msg.peer.kind, PeerKind::Direct);
        assert_eq!(msg.peer.id, "u123");
        assert_eq!(msg.text, Some("hello".to_string()));
        assert!(!msg.ambient);
    }

    #[test]
    fn should_serialize_and_deserialize_message() {
        let msg = InboundMessage::direct("telegram", "default", "u123", "hello");
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: InboundMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(msg, decoded);
    }
}
