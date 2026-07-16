use async_trait::async_trait;
use legion_plugin_sdk::channel::{
    ChannelCapabilities, ChannelError, ChannelProvider, InboundMessage, OutboundMessage,
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Built-in WebChat channel provider.
///
/// This provider does not connect to an external API. Instead, it exposes an
/// inbound [`mpsc`] channel that the Gateway WebSocket handler can use to inject
/// messages from the Web UI. Outbound messages are stored in a shared queue so
/// the Web UI can poll them.
#[derive(Debug)]
pub struct WebChatProvider {
    /// Sender passed to `start`. Used by `inject` to push messages into the
    /// Gateway router.
    inbound_tx: Mutex<Option<mpsc::Sender<InboundMessage>>>,
    /// Outbound messages queued by `send` and drained by the Web UI.
    outbound: Arc<Mutex<Vec<OutboundMessage>>>,
}

impl WebChatProvider {
    pub fn new() -> Self {
        Self {
            inbound_tx: Mutex::new(None),
            outbound: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Inject an inbound message into the Gateway router.
    ///
    /// This is called by the Gateway WebSocket handler when the Web UI sends a
    /// message.
    pub async fn inject(&self, message: InboundMessage) -> Result<(), ChannelError> {
        let tx = self.inbound_tx.lock().await.clone();
        match tx {
            Some(tx) => tx
                .send(message)
                .await
                .map_err(|_| ChannelError::Runtime("inbound channel closed".into())),
            None => Err(ChannelError::NotStarted),
        }
    }

    /// Drain all outbound messages queued by `send`.
    ///
    /// The Web UI polls this to display replies from the agent.
    pub async fn drain_outbound(&self) -> Vec<OutboundMessage> {
        std::mem::take(&mut *self.outbound.lock().await)
    }

    /// Peek at the number of outbound messages currently queued.
    pub async fn outbound_len(&self) -> usize {
        self.outbound.lock().await.len()
    }
}

impl Default for WebChatProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelProvider for WebChatProvider {
    fn channel_id(&self) -> &str {
        "webchat"
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
            group: false,
            thread: false,
            reactions: false,
            typing: false,
        }
    }

    async fn start(
        &self,
        _config: serde_json::Value,
        inbound_tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), ChannelError> {
        *self.inbound_tx.lock().await = Some(inbound_tx);
        tracing::info!(channel = "webchat", "WebChat channel started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChannelError> {
        *self.inbound_tx.lock().await = None;
        tracing::info!(channel = "webchat", "WebChat channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        self.outbound.lock().await.push(message);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_plugin_sdk::channel::{Peer, PeerKind, Sender};

    fn sample_inbound(text: &str) -> InboundMessage {
        InboundMessage {
            channel: "webchat".into(),
            account_id: "default".into(),
            peer: Peer {
                kind: PeerKind::Direct,
                id: "user-1".into(),
                name: None,
                thread_id: None,
            },
            sender: Sender {
                id: "user-1".into(),
                display_name: Some("Test User".into()),
                username: None,
            },
            message_id: "msg-1".into(),
            text: Some(text.into()),
            media: vec![],
            reply_to: None,
            timestamp: "2026-07-08T14:00:00Z".into(),
            is_mentioned: false,
            ambient: false,
            guild_id: None,
            team_id: None,
        }
    }

    #[tokio::test]
    async fn should_queue_outbound_message() {
        let provider = WebChatProvider::new();
        let outbound = OutboundMessage {
            channel: "webchat".into(),
            account_id: "default".into(),
            peer: Peer {
                kind: PeerKind::Direct,
                id: "user-1".into(),
                name: None,
                thread_id: None,
            },
            text: Some("hello back".into()),
            media: vec![],
            reply_to: None,
        };

        provider.send(outbound.clone()).await.unwrap();
        assert_eq!(provider.outbound_len().await, 1);

        let drained = provider.drain_outbound().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0], outbound);
        assert_eq!(provider.outbound_len().await, 0);
    }

    #[tokio::test]
    async fn should_reject_inject_before_start() {
        let provider = WebChatProvider::new();
        let result = provider.inject(sample_inbound("hi")).await;
        assert_eq!(result, Err(ChannelError::NotStarted));
    }

    #[tokio::test]
    async fn should_inject_inbound_message_to_gateway() {
        let provider = WebChatProvider::new();
        let (tx, mut rx) = mpsc::channel(8);

        provider.start(serde_json::Value::Null, tx).await.unwrap();

        let inbound = sample_inbound("hi from web ui");
        provider.inject(inbound.clone()).await.unwrap();

        let received = rx.recv().await.expect("message should be routed");
        assert_eq!(received.channel, "webchat");
        assert_eq!(received.text, Some("hi from web ui".into()));
        assert_eq!(received.peer.kind, PeerKind::Direct);
    }

    #[tokio::test]
    async fn should_report_webchat_capabilities() {
        let provider = WebChatProvider::new();
        assert_eq!(provider.channel_id(), "webchat");
        let caps = provider.capabilities();
        assert!(caps.text);
        assert!(!caps.group);
    }
}
