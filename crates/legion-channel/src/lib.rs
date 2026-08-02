pub mod access;
pub mod discord;
pub mod lark;
pub mod matrix;
pub mod slack;
pub mod telegram;
mod util;
pub mod webchat;

pub use discord::DiscordProvider;
pub use lark::LarkProvider;
pub use matrix::MatrixProvider;
pub use slack::SlackProvider;
pub use telegram::TelegramProvider;
pub use webchat::WebChatProvider;

use async_trait::async_trait;
use legion_plugin_sdk::channel::{ChannelProvider, InboundMessage, OutboundMessage, PeerKind};
use legion_runtime::{ApprovalNotifier, ApprovalRequest};
use std::sync::Arc;
use tracing::warn;

/// Notifier that sends an approval request back through the originating
/// channel provider as a text message.
pub struct ChannelApprovalNotifier {
    channel: String,
    account_id: String,
    peer: legion_plugin_sdk::channel::Peer,
    provider: Arc<dyn ChannelProvider>,
}

impl ChannelApprovalNotifier {
    pub fn new(
        channel: String,
        account_id: String,
        peer: legion_plugin_sdk::channel::Peer,
        provider: Arc<dyn ChannelProvider>,
    ) -> Self {
        Self {
            channel,
            account_id,
            peer,
            provider,
        }
    }
}

#[async_trait]
impl ApprovalNotifier for ChannelApprovalNotifier {
    async fn notify(&self, req: &ApprovalRequest, prompt_id: &str) {
        if self.provider.capabilities().buttons {
            match self
                .provider
                .send_approval_card(&self.peer, &req.tool, prompt_id)
                .await
            {
                Ok(true) => return,
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        channel = %self.channel,
                        error = %err,
                        "failed to send approval card; falling back to text"
                    );
                }
            }
        }
        let text = format!(
            "Approval required for tool '{}'. Reply 'approve:{}' to allow or 'deny:{}' to refuse.",
            req.tool, prompt_id, prompt_id
        );
        let outbound = OutboundMessage {
            channel: self.channel.clone(),
            account_id: self.account_id.clone(),
            peer: self.peer.clone(),
            text: Some(text),
            media: vec![],
            reply_to: None,
        };
        if let Err(err) = self.provider.send(outbound).await {
            warn!(
                channel = %self.channel,
                error = %err,
                "failed to send approval request outbound"
            );
        }
    }
}

/// Parse an inbound text message as an approval reply.
///
/// Supported formats:
/// - `approve:<prompt_id>`
/// - `deny:<prompt_id>`
///
/// Returns `Some((prompt_id, allow))` when the message is a well-formed reply.
pub fn parse_approval_reply(text: &str) -> Option<(&str, bool)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("approve:") {
        return Some((rest.trim(), true));
    }
    if let Some(rest) = text.strip_prefix("deny:") {
        return Some((rest.trim(), false));
    }
    None
}

/// Helper to build a minimal WebChat inbound message for tests and Gateway handlers.
pub fn webchat_inbound(peer_id: impl Into<String>, text: impl Into<String>) -> InboundMessage {
    let peer_id = peer_id.into();
    InboundMessage {
        channel: "webchat".into(),
        account_id: "default".into(),
        peer: legion_plugin_sdk::channel::Peer {
            kind: PeerKind::Direct,
            id: peer_id.clone(),
            name: None,
            thread_id: None,
        },
        sender: legion_plugin_sdk::channel::Sender {
            id: peer_id,
            display_name: None,
            username: None,
        },
        message_id: format!("msg-{}", legion_core::util::next_id()),
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
    use async_trait::async_trait;
    use legion_plugin_sdk::channel::{
        ChannelCapabilities, ChannelError, OutboundMessage, Peer, PeerKind,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::mpsc;

    struct RecordingProvider {
        tx: mpsc::UnboundedSender<OutboundMessage>,
        stopped: AtomicBool,
    }

    #[async_trait]
    impl ChannelProvider for RecordingProvider {
        fn channel_id(&self) -> &str {
            "recording"
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                text: true,
                ..Default::default()
            }
        }

        async fn start(
            &self,
            _config: serde_json::Value,
            _inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
        ) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn stop(&self) -> Result<(), ChannelError> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
            self.tx
                .send(message)
                .map_err(|_| ChannelError::SendFailed("receiver dropped".to_string()))
        }
    }

    #[test]
    fn parse_approval_reply_recognizes_approve() {
        assert_eq!(parse_approval_reply("approve:p1"), Some(("p1", true)));
        assert_eq!(
            parse_approval_reply("  approve:prompt-42  "),
            Some(("prompt-42", true))
        );
    }

    #[test]
    fn parse_approval_reply_recognizes_deny() {
        assert_eq!(parse_approval_reply("deny:p2"), Some(("p2", false)));
        assert_eq!(
            parse_approval_reply("deny: prompt-3"),
            Some(("prompt-3", false))
        );
    }

    #[test]
    fn parse_approval_reply_returns_none_for_normal_text() {
        assert_eq!(parse_approval_reply("hello"), None);
        assert_eq!(parse_approval_reply("approved:p1"), None);
        assert_eq!(parse_approval_reply("approve"), None);
    }

    #[tokio::test]
    async fn channel_approval_notifier_sends_prompt_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider: Arc<dyn ChannelProvider> = Arc::new(RecordingProvider {
            tx,
            stopped: AtomicBool::new(false),
        });
        let notifier = ChannelApprovalNotifier::new(
            "webchat".into(),
            "default".into(),
            Peer {
                kind: PeerKind::Direct,
                id: "u1".into(),
                name: None,
                thread_id: None,
            },
            provider,
        );

        let req = ApprovalRequest {
            tool: "exec".into(),
            agent_id: "main".into(),
            session_key: "agent:main:dm:webchat:default:direct:u1".into(),
            interactive: true,
        };
        notifier.notify(&req, "prompt-7").await;

        let outbound = rx
            .recv()
            .await
            .expect("notifier should send outbound message");
        assert_eq!(outbound.channel, "webchat");
        assert_eq!(outbound.account_id, "default");
        assert_eq!(outbound.peer.id, "u1");
        let text = outbound.text.expect("outbound should have text");
        assert!(text.contains("exec"));
        assert!(text.contains("approve:prompt-7"));
        assert!(text.contains("deny:prompt-7"));
    }

    /// Provider that advertises button support and answers card sends with a
    /// configurable result, while still recording any text fallback.
    struct CardProvider {
        tx: mpsc::UnboundedSender<OutboundMessage>,
        card_ok: bool,
        card_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ChannelProvider for CardProvider {
        fn channel_id(&self) -> &str {
            "card"
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                text: true,
                buttons: true,
                ..Default::default()
            }
        }

        async fn start(
            &self,
            _config: serde_json::Value,
            _inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
        ) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn stop(&self) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
            self.tx
                .send(message)
                .map_err(|_| ChannelError::SendFailed("receiver dropped".to_string()))
        }

        async fn send_approval_card(
            &self,
            _peer: &Peer,
            _tool: &str,
            _prompt_id: &str,
        ) -> Result<bool, ChannelError> {
            self.card_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.card_ok)
        }
    }

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            tool: "exec".into(),
            agent_id: "main".into(),
            session_key: "agent:main:dm:lark:default:direct:u1".into(),
            interactive: true,
        }
    }

    fn dm_peer() -> Peer {
        Peer {
            kind: PeerKind::Direct,
            id: "u1".into(),
            name: None,
            thread_id: None,
        }
    }

    #[tokio::test]
    async fn notifier_uses_card_when_provider_supports_buttons() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(CardProvider {
            tx,
            card_ok: true,
            card_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let notifier = ChannelApprovalNotifier::new(
            "lark".into(),
            "default".into(),
            dm_peer(),
            provider.clone(),
        );

        notifier.notify(&approval_request(), "prompt-3").await;

        assert_eq!(provider.card_calls.load(Ordering::SeqCst), 1);
        // Card accepted: no text fallback must be sent.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn notifier_falls_back_to_text_when_card_unsupported() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(CardProvider {
            tx,
            card_ok: false,
            card_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let notifier = ChannelApprovalNotifier::new(
            "lark".into(),
            "default".into(),
            dm_peer(),
            provider.clone(),
        );

        notifier.notify(&approval_request(), "prompt-4").await;

        assert_eq!(provider.card_calls.load(Ordering::SeqCst), 1);
        let outbound = rx.recv().await.expect("text fallback should be sent");
        let text = outbound.text.expect("fallback should have text");
        assert!(text.contains("approve:prompt-4"));
    }
}
