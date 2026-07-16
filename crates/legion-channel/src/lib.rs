pub mod access;
pub mod discord;
pub mod lark;
pub mod matrix;
pub mod slack;
pub mod telegram;
pub mod webchat;

pub use discord::DiscordProvider;
pub use lark::LarkProvider;
pub use matrix::MatrixProvider;
pub use slack::SlackProvider;
pub use telegram::TelegramProvider;
pub use webchat::WebChatProvider;

use async_trait::async_trait;
use futures::StreamExt;
use legion_core::config::Config;
use legion_plugin_sdk::channel::{ChannelProvider, InboundMessage, OutboundMessage, PeerKind};
use legion_runtime::{
    ApprovalNotifier, ApprovalQueueRegistry, ApprovalRequest, Harness, RunEvent, RunRequest,
};
use std::sync::Arc;
use tracing::{error, info, warn};

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

/// Route an inbound channel message to the agent runtime and send the reply
/// back through the originating channel provider.
///
/// `resolver` receives the inbound message and must return the target
/// `agent_id`. This keeps the routing engine in `legion-gateway` while still
/// allowing `legion-channel` to build the canonical session key.
///
/// If `approval_registry` is provided, inbound messages that look like approval
/// replies (`approve:<prompt_id>` / `deny:<prompt_id>`) are resolved through
/// the registry and are not routed to the runtime.
///
/// Access control (channels gap Phase A): every message is evaluated against
/// `channels.<id>.access` (default: allowlist DMs + requireMention in groups)
/// and, when a `bot_guard` is provided, denied on suspected reply loops.
///
/// After access control passes, providers advertising the `typing` capability
/// get a typing-indicator loop (refreshed every 4s until the run finishes and
/// the reply is sent) and providers advertising `reactions` get a one-shot
/// 👀 reaction on the inbound message. Providers without those capabilities
/// are unaffected.
pub async fn route_inbound_to_runtime(
    runtime: Arc<dyn Harness>,
    config: Config,
    resolver: Arc<dyn Fn(&InboundMessage) -> String + Send + Sync>,
    channel_registry: Arc<legion_plugin_sdk::PluginRegistry>,
    approval_registry: Option<Arc<ApprovalQueueRegistry>>,
    bot_guard: Option<Arc<access::BotLoopGuard>>,
    msg: InboundMessage,
) {
    // First, handle approval replies so they do not start a new agent turn.
    if let Some(text) = msg.text.as_deref() {
        if let Some((prompt_id, allow)) = parse_approval_reply(text) {
            if let Some(registry) = approval_registry {
                let resolved = registry.resolve(prompt_id, allow).await;
                if resolved {
                    info!(prompt_id, allow, "approval reply resolved");
                } else {
                    warn!(prompt_id, "approval reply did not match any pending prompt");
                }
            }
            return;
        }
    }

    // Access control: enforce dmPolicy / group policies before any routing.
    let policy = access::policy_for(&config, &msg.channel);
    match access::evaluate(&msg, &policy) {
        access::AccessDecision::Allow => {}
        access::AccessDecision::Deny(reason) => {
            warn!(
                channel = %msg.channel,
                sender = %msg.sender.id,
                peer = %msg.peer.id,
                ?reason,
                "inbound message denied by access policy (configure channels.{}.access to allow)",
                msg.channel
            );
            return;
        }
        access::AccessDecision::RequireMention => {
            info!(
                channel = %msg.channel,
                peer = %msg.peer.id,
                "group message without mention ignored (requireMention)"
            );
            return;
        }
    }
    if let Some(guard) = &bot_guard {
        if !guard.check_inbound(&msg.channel, &msg.peer.id) {
            warn!(
                channel = %msg.channel,
                peer = %msg.peer.id,
                "inbound denied: suspected bot loop (too many recent replies)"
            );
            return;
        }
    }

    // Single provider lookup reused for the typing indicator, reaction,
    // approval gate, and the outbound reply below.
    let provider = channel_registry.channel(&msg.channel);

    if let Some(provider) = provider.as_ref() {
        if provider.capabilities().reactions {
            let provider = provider.clone();
            let peer = msg.peer.clone();
            let message_id = msg.message_id.clone();
            tokio::spawn(async move {
                if let Err(err) = provider.add_reaction(&peer, &message_id, "👀").await {
                    warn!(error = %err, "failed to add inbound reaction");
                }
            });
        }
    }

    // Typing indicator loop (gated by channel capabilities): Telegram's
    // typing action expires after ~5s, so refresh every 4s until the run
    // finishes and the reply has been sent.
    let typing_stop_tx = if let Some(provider) = provider
        .as_ref()
        .filter(|provider| provider.capabilities().typing)
    {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let provider = provider.clone();
        let peer = msg.peer.clone();
        tokio::spawn(async move {
            loop {
                if let Err(err) = provider.send_typing(&peer).await {
                    warn!(error = %err, "failed to send typing indicator");
                }
                tokio::select! {
                    _ = rx.changed() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {}
                }
            }
        });
        Some(tx)
    } else {
        None
    };

    let agent_id = resolver(&msg);
    let session_id = build_session_key(&agent_id, &config.session.dm_scope, &msg);
    let model_ref = "openai/gpt-4o".to_string(); // MVP default.

    // Build a channel-specific approval gate so Prompt/Required tools can ask
    // the originating user for permission and receive their reply.
    let approval_gate = provider.clone().map(|provider| {
        let notifier = Arc::new(ChannelApprovalNotifier::new(
            msg.channel.clone(),
            msg.account_id.clone(),
            msg.peer.clone(),
            provider,
        ));
        let gate = legion_runtime::ApprovalGate::new(notifier, std::time::Duration::from_secs(300));
        let gate = match approval_registry {
            Some(ref registry) => gate.with_registry(registry.clone()),
            None => gate,
        };
        Arc::new(gate)
    });

    let user_message = msg.text.clone().unwrap_or_default();
    let mut request = RunRequest::new(
        session_id.clone(),
        agent_id.clone(),
        user_message,
        model_ref,
    )
    .with_sender(msg.sender.id.clone())
    .with_interactive(true);
    if let Some(gate) = approval_gate {
        request = request.with_approval_gate(gate);
    }

    info!(
        channel = %msg.channel,
        account = %msg.account_id,
        session = %session_id,
        "routing inbound message to runtime"
    );

    let stream = match runtime.run(request) {
        Ok(s) => s,
        Err(err) => {
            error!(error = %err, "failed to start agent run");
            if let Some(tx) = typing_stop_tx.as_ref() {
                let _ = tx.send(true);
            }
            return;
        }
    };

    tokio::spawn(async move {
        let mut reply_text = String::new();
        let mut reply_to: Option<String> = None;

        futures::pin_mut!(stream);
        while let Some(event) = stream.next().await {
            match event {
                RunEvent::AssistantDelta { delta } => reply_text.push_str(&delta),
                RunEvent::Lifecycle { phase, .. }
                    if phase == legion_runtime::LifecyclePhase::End && !reply_text.is_empty() =>
                {
                    reply_to = Some(msg.message_id.clone());
                }
                _ => {}
            }
        }

        if !reply_text.is_empty() {
            let outbound = OutboundMessage {
                channel: msg.channel.clone(),
                account_id: msg.account_id.clone(),
                peer: msg.peer.clone(),
                text: Some(reply_text),
                media: vec![],
                reply_to,
            };

            if let Some(provider) = provider.as_ref() {
                if let Err(err) = provider.send(outbound).await {
                    warn!(channel = %msg.channel, error = %err, "failed to send outbound reply");
                } else if let Some(guard) = &bot_guard {
                    guard.record_outbound(&msg.channel, &msg.peer.id);
                }
            } else {
                warn!(channel = %msg.channel, "no provider found for outbound reply");
            }
        }

        if let Some(tx) = typing_stop_tx {
            let _ = tx.send(true);
        }
    });
}

fn build_session_key(agent_id: &str, scope: &str, msg: &InboundMessage) -> String {
    let peer_kind = match msg.peer.kind {
        PeerKind::Direct => "direct",
        PeerKind::Group => "group",
        PeerKind::Thread => "thread",
    };
    format!(
        "agent:{}:{}:{}:{}:{}:{}",
        agent_id, scope, msg.channel, msg.account_id, peer_kind, msg.peer.id
    )
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
        message_id: format!("msg-{}", uuid_like()),
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

fn uuid_like() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
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
        ChannelCapabilities, ChannelError, OutboundMessage, Peer, PeerKind, Sender,
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
    fn should_build_session_key_for_dm() {
        let msg = InboundMessage {
            channel: "telegram".into(),
            account_id: "default".into(),
            peer: Peer {
                kind: PeerKind::Direct,
                id: "123".into(),
                name: None,
                thread_id: None,
            },
            sender: Sender {
                id: "123".into(),
                display_name: None,
                username: None,
            },
            message_id: "m1".into(),
            text: Some("hi".into()),
            media: vec![],
            reply_to: None,
            timestamp: "t".into(),
            is_mentioned: false,
            ambient: false,
            guild_id: None,
            team_id: None,
        };

        assert_eq!(
            build_session_key("main", "main", &msg),
            "agent:main:main:telegram:default:direct:123"
        );
    }

    #[test]
    fn should_build_session_key_for_group() {
        let msg = InboundMessage {
            channel: "telegram".into(),
            account_id: "default".into(),
            peer: Peer {
                kind: PeerKind::Group,
                id: "g1".into(),
                name: None,
                thread_id: None,
            },
            sender: Sender {
                id: "u1".into(),
                display_name: None,
                username: None,
            },
            message_id: "m1".into(),
            text: Some("hi".into()),
            media: vec![],
            reply_to: None,
            timestamp: "t".into(),
            is_mentioned: false,
            ambient: false,
            guild_id: None,
            team_id: None,
        };

        assert_eq!(
            build_session_key("work", "main", &msg),
            "agent:work:main:telegram:default:group:g1"
        );
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

    use legion_plugin_sdk::PluginRegistry;
    use legion_runtime::{RunStream, RuntimeError};
    use std::sync::atomic::AtomicUsize;

    /// Provider advertising typing + reactions, counting how often each
    /// capability is exercised by the router.
    struct TypingRecordingProvider {
        typing_calls: AtomicUsize,
        reaction_calls: AtomicUsize,
    }

    impl TypingRecordingProvider {
        fn new() -> Self {
            Self {
                typing_calls: AtomicUsize::new(0),
                reaction_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ChannelProvider for TypingRecordingProvider {
        fn channel_id(&self) -> &str {
            "typingrec"
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                text: true,
                reactions: true,
                typing: true,
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

        async fn send(&self, _message: OutboundMessage) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn send_typing(&self, _peer: &Peer) -> Result<(), ChannelError> {
            self.typing_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn add_reaction(
            &self,
            _peer: &Peer,
            _message_id: &str,
            _emoji: &str,
        ) -> Result<(), ChannelError> {
            self.reaction_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Minimal harness returning a fixed event stream.
    struct StubHarness {
        events: Vec<RunEvent>,
    }

    #[async_trait]
    impl Harness for StubHarness {
        fn id(&self) -> &str {
            "stub"
        }

        fn can_handle(&self, _model_ref: &str) -> bool {
            true
        }

        fn run(&self, _request: RunRequest) -> Result<RunStream, RuntimeError> {
            Ok(Box::pin(futures::stream::iter(self.events.clone())))
        }
    }

    fn open_dm_config(channel: &str) -> Config {
        let json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "channels": {{ "{channel}": {{ "access": {{ "dmPolicy": "open" }} }} }}
            }}"#
        );
        Config::from_json(&json).expect("config should parse")
    }

    fn stub_resolver() -> Arc<dyn Fn(&InboundMessage) -> String + Send + Sync> {
        Arc::new(|_msg: &InboundMessage| "main".to_string())
    }

    #[tokio::test(start_paused = true)]
    async fn route_triggers_typing_and_reaction_when_capabilities_advertise() {
        let provider = Arc::new(TypingRecordingProvider::new());
        let mut registry = PluginRegistry::new();
        registry
            .register_channel("typingrec", provider.clone())
            .expect("channel should register");

        let runtime: Arc<dyn Harness> = Arc::new(StubHarness { events: vec![] });
        route_inbound_to_runtime(
            runtime,
            open_dm_config("typingrec"),
            stub_resolver(),
            Arc::new(registry),
            None,
            None,
            InboundMessage::direct("typingrec", "default", "u1", "hello"),
        )
        .await;

        // Let the spawned reaction / typing / reply tasks run.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            provider.typing_calls.load(Ordering::SeqCst) >= 1,
            "typing indicator should be sent at least once"
        );
        assert_eq!(
            provider.reaction_calls.load(Ordering::SeqCst),
            1,
            "reaction should be sent exactly once"
        );

        // Advance well past several 4s refresh intervals: the typing loop
        // must have stopped when the (empty) run stream ended.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(
            provider.typing_calls.load(Ordering::SeqCst),
            1,
            "typing loop should stop after the run stream ends"
        );
    }

    #[tokio::test]
    async fn route_without_typing_capabilities_still_sends_reply() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider: Arc<dyn ChannelProvider> = Arc::new(RecordingProvider {
            tx,
            stopped: AtomicBool::new(false),
        });
        let mut registry = PluginRegistry::new();
        registry
            .register_channel("recording", provider)
            .expect("channel should register");

        let runtime: Arc<dyn Harness> = Arc::new(StubHarness {
            events: vec![
                RunEvent::AssistantDelta {
                    delta: "hi there".into(),
                },
                RunEvent::Lifecycle {
                    phase: legion_runtime::LifecyclePhase::End,
                    error: None,
                },
            ],
        });
        route_inbound_to_runtime(
            runtime,
            open_dm_config("recording"),
            stub_resolver(),
            Arc::new(registry),
            None,
            None,
            InboundMessage::direct("recording", "default", "u1", "hello"),
        )
        .await;

        let outbound = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("reply should be sent promptly")
            .expect("channel should stay open");
        assert_eq!(outbound.text.as_deref(), Some("hi there"));
    }

    /// Harness that only counts how many runs were started.
    struct CountingHarness {
        runs: AtomicUsize,
    }

    #[async_trait]
    impl Harness for CountingHarness {
        fn id(&self) -> &str {
            "counting"
        }

        fn can_handle(&self, _model_ref: &str) -> bool {
            true
        }

        fn run(&self, _request: RunRequest) -> Result<RunStream, RuntimeError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    fn counting_harness() -> Arc<CountingHarness> {
        Arc::new(CountingHarness {
            runs: AtomicUsize::new(0),
        })
    }

    /// Registry with the recording channel wired to an outbound receiver.
    fn recording_registry() -> (PluginRegistry, mpsc::UnboundedReceiver<OutboundMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let provider: Arc<dyn ChannelProvider> = Arc::new(RecordingProvider {
            tx,
            stopped: AtomicBool::new(false),
        });
        let mut registry = PluginRegistry::new();
        registry
            .register_channel("recording", provider)
            .expect("channel should register");
        (registry, rx)
    }

    #[tokio::test]
    async fn route_denies_dm_not_in_allowlist() {
        let harness = counting_harness();
        let (registry, mut rx) = recording_registry();
        // No `access` block: dmPolicy defaults to allowlist with an empty
        // allowlist, so a stranger's DM must be denied before routing.
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#)
            .expect("config should parse");

        route_inbound_to_runtime(
            harness.clone(),
            config,
            stub_resolver(),
            Arc::new(registry),
            None,
            None,
            InboundMessage::direct("recording", "default", "stranger", "hello"),
        )
        .await;

        assert_eq!(
            harness.runs.load(Ordering::SeqCst),
            0,
            "denied DM must not reach the runtime"
        );
        assert!(
            rx.try_recv().is_err(),
            "denied DM must not produce an outbound reply"
        );
    }

    #[tokio::test]
    async fn route_ignores_group_message_without_mention() {
        let harness = counting_harness();
        let (registry, mut rx) = recording_registry();
        // dmPolicy is open, but groups.requireMention defaults to true.
        let mut msg = InboundMessage::direct("recording", "default", "u1", "hello all");
        msg.peer = Peer {
            kind: PeerKind::Group,
            id: "g1".into(),
            name: None,
            thread_id: None,
        };
        // is_mentioned stays false.

        route_inbound_to_runtime(
            harness.clone(),
            open_dm_config("recording"),
            stub_resolver(),
            Arc::new(registry),
            None,
            None,
            msg,
        )
        .await;

        assert_eq!(
            harness.runs.load(Ordering::SeqCst),
            0,
            "unmentioned group message must not reach the runtime"
        );
        assert!(
            rx.try_recv().is_err(),
            "ignored group message must not produce an outbound reply"
        );
    }

    #[tokio::test]
    async fn route_denies_after_bot_loop_guard_trips() {
        let harness = counting_harness();
        let guard = Arc::new(access::BotLoopGuard::new(
            std::time::Duration::from_secs(3600),
            2,
        ));

        // Baseline: an allowed DM reaches the runtime.
        let (registry, _rx) = recording_registry();
        route_inbound_to_runtime(
            harness.clone(),
            open_dm_config("recording"),
            stub_resolver(),
            Arc::new(registry),
            None,
            Some(guard.clone()),
            InboundMessage::direct("recording", "default", "u1", "hi"),
        )
        .await;
        assert_eq!(harness.runs.load(Ordering::SeqCst), 1);

        // Trip the guard with two recent outbound replies to the same peer.
        guard.record_outbound("recording", "u1");
        guard.record_outbound("recording", "u1");
        assert!(!guard.check_inbound("recording", "u1"));

        // Subsequent inbound from that peer is denied before the runtime.
        let (registry, _rx) = recording_registry();
        route_inbound_to_runtime(
            harness.clone(),
            open_dm_config("recording"),
            stub_resolver(),
            Arc::new(registry),
            None,
            Some(guard),
            InboundMessage::direct("recording", "default", "u1", "are you stuck?"),
        )
        .await;
        assert_eq!(
            harness.runs.load(Ordering::SeqCst),
            1,
            "suspected bot loop must not reach the runtime"
        );
    }
}
