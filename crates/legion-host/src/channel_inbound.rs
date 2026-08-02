//! Channel inbound routing: channel-side concerns (approval replies, access
//! control, bot-loop guard, typing/reaction capabilities) followed by the
//! shared turn pipeline.
//!
//! This is the channel counterpart of the gateway's WS `agent` RPC: once a
//! message passes access control it goes through
//! [`crate::turn::prepare_run`] (history load + orphan repair + configured
//! model resolution) and [`crate::turn::drive_run_stream`] (transcript
//! persistence + compaction boundaries), while the run's assistant deltas
//! are collected and sent back through the originating channel provider.
//!
//! It lives in `legion-host` (not `legion-channel`) because the dependency
//! direction is `legion-host` → `legion-channel`; the channel crate cannot
//! call the turn pipeline without creating a cycle.

use crate::routing::Router;
use crate::session::SessionStore;
use crate::turn::{drive_run_stream, prepare_run};
use legion_channel::access::{self, BotLoopGuard};
use legion_channel::{ChannelApprovalNotifier, parse_approval_reply};
use legion_core::config::Config;
use legion_plugin_sdk::PluginRegistry;
use legion_plugin_sdk::channel::{InboundMessage, OutboundMessage};
use legion_plugin_sdk::session_key::{SessionKeyParts, build_session_key};
use legion_protocol::{AgentParams, UserMessage, WsFrame};
use legion_runtime::{ApprovalGate, ApprovalQueueRegistry, Harness};
use std::sync::Arc;
use tracing::{error, info, warn};

/// Route an inbound channel message through the shared turn pipeline and
/// send the reply back through the originating channel provider.
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
///
/// The run itself reuses the WS `agent` RPC pipeline: the session transcript
/// is loaded (with orphan repair) and persisted, and the model is resolved
/// from config via `resolve_agent_model` — channel sessions are resumable
/// instead of starting empty on every message.
#[allow(clippy::too_many_arguments)]
pub async fn route_inbound_to_runtime(
    runtime: Arc<dyn Harness>,
    config: Config,
    router: Arc<Router>,
    session_store: Arc<SessionStore>,
    channel_registry: Arc<PluginRegistry>,
    approval_registry: Option<Arc<ApprovalQueueRegistry>>,
    bot_guard: Option<Arc<BotLoopGuard>>,
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

    // Whether this provider can show a "processing" reaction. The reaction is
    // added when the run starts and removed once the reply is sent.
    let supports_reaction = provider
        .as_ref()
        .is_some_and(|provider| provider.capabilities().reactions);

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

    let agent_id = router.resolve_agent(&msg);
    let session_key = channel_session_key(&agent_id, &config.session.dm_scope, &msg);
    let user_content = msg.text.clone().unwrap_or_default();

    // Build a channel-specific approval gate so Prompt/Required tools can ask
    // the originating user for permission and receive their reply.
    let approval_gate = provider.clone().map(|provider| {
        let notifier = Arc::new(ChannelApprovalNotifier::new(
            msg.channel.clone(),
            msg.account_id.clone(),
            msg.peer.clone(),
            provider,
        ));
        let gate = ApprovalGate::new(notifier, std::time::Duration::from_secs(300));
        let gate = match approval_registry {
            Some(ref registry) => gate.with_registry(registry.clone()),
            None => gate,
        };
        Arc::new(gate)
    });

    info!(
        channel = %msg.channel,
        account = %msg.account_id,
        session = %session_key,
        message_id = %msg.message_id,
        sender = %msg.sender.id,
        text_len = msg.text.as_deref().map(str::len).unwrap_or(0),
        "routing inbound message to runtime"
    );

    let params = AgentParams {
        session_key,
        message: UserMessage {
            role: "user".to_string(),
            content: user_content.clone(),
        },
        idempotency_key: None,
        wait: false,
        history: Vec::new(),
        dump_prompts: false,
        yolo: false,
        workspace: None,
        sender: Some(msg.sender.id.clone()),
    };

    // Resume prep (history load + orphan repair + model resolution + run
    // start) is shared with the WS `agent` RPC via `turn::prepare_run`.
    let (stream, accepted, session_key) = match prepare_run(
        &*runtime,
        &config,
        &router,
        &session_store,
        params,
        approval_gate,
        None,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(err) => {
            error!(error = %err, "failed to start agent run");
            if let Some(tx) = typing_stop_tx.as_ref() {
                let _ = tx.send(true);
            }
            return;
        }
    };

    let channel = msg.channel.clone();
    let account_id = msg.account_id.clone();
    let peer = msg.peer.clone();
    let message_id = msg.message_id.clone();
    tokio::spawn(async move {
        // Mark the inbound message as "processing" before the run starts; the
        // reaction is cleared once the reply is sent below.
        if supports_reaction {
            if let Some(provider) = provider.as_ref() {
                if let Err(err) = provider.add_reaction(&peer, &message_id, "⏳").await {
                    warn!(error = %err, "failed to add inbound reaction");
                }
            }
        }

        let mut reply_text = String::new();
        let mut reply_to: Option<String> = None;

        // `drive_run_stream` persists the transcript and forwards each event
        // as an `agent` frame; the emit closure collects the assistant text
        // (and the terminal phase) for the channel reply.
        let emit = |frame: WsFrame| {
            if let WsFrame::Event { payload, .. } = &frame {
                match payload.get("stream").and_then(|s| s.as_str()) {
                    Some("assistant") => {
                        if let Some(delta) = payload.get("delta").and_then(|d| d.as_str()) {
                            reply_text.push_str(delta);
                        }
                    }
                    Some("lifecycle")
                        if payload.get("phase").and_then(|p| p.as_str()) == Some("end")
                            && !reply_text.is_empty() =>
                    {
                        reply_to = Some(message_id.clone());
                    }
                    _ => {}
                }
            }
        };
        if let Err(err) = drive_run_stream(
            stream,
            session_store,
            session_key,
            user_content,
            accepted.run_id,
            emit,
        )
        .await
        {
            error!(error = %err, "failed to persist session transcript");
        }

        if !reply_text.is_empty() {
            // Reasoning blocks inline as `<think>` in the content stream must
            // not leak into the IM reply; strip them before sending.
            let reply_text = strip_think_blocks(&reply_text);
            if reply_text.is_empty() {
                // The reply was reasoning-only (e.g. the model only produced a
                // think block); nothing to send.
                info!(
                    "agent run produced no reply text after stripping think blocks; skipping outbound"
                );
            } else {
                let outbound = OutboundMessage {
                    channel: channel.clone(),
                    account_id,
                    peer: peer.clone(),
                    text: Some(reply_text),
                    media: vec![],
                    reply_to,
                };

                if let Some(provider) = provider.as_ref() {
                    if let Err(err) = provider.send(outbound).await {
                        warn!(channel = %channel, error = %err, "failed to send outbound reply");
                    } else if let Some(guard) = &bot_guard {
                        guard.record_outbound(&channel, &peer.id);
                    }
                } else {
                    warn!(channel = %channel, "no provider found for outbound reply");
                }
            }
        }

        if let Some(tx) = typing_stop_tx {
            let _ = tx.send(true);
        }

        // Always clear the "processing" reaction once the turn is over,
        // regardless of whether a reply was sent — leaving it up would imply
        // the run is still in progress.
        if supports_reaction {
            if let Some(provider) = provider.as_ref() {
                if let Err(err) = provider.remove_reaction(&peer, &message_id, "⏳").await {
                    warn!(error = %err, "failed to remove inbound reaction");
                }
            }
        }
    });
}

/// Build the canonical session key for a channel inbound message. The agent
/// segment is provisional: `turn::prepare_run` re-resolves the agent from the
/// key parts against the configured bindings and rebuilds the key.
fn channel_session_key(agent_id: &str, scope: &str, msg: &InboundMessage) -> String {
    let parts = SessionKeyParts::new(
        agent_id,
        scope,
        msg.channel.clone(),
        msg.account_id.clone(),
        msg.peer.kind.clone(),
        msg.peer.id.clone(),
    );
    build_session_key(agent_id, &parts)
}

/// Strip `<think>...</think>` reasoning blocks from an assistant reply before
/// sending it over a channel.
///
/// Some models (e.g. MiniMax-M3 via the OpenAI-compatible interface) inline
/// their reasoning inside `<think>` tags in the content stream rather than a
/// separate `reasoning_content` field. Channels forward that raw content, so
/// without stripping the reasoning leaks into IM replies. The TUI hides this
/// at render time; channels need the text removed up front.
///
/// Mirrors the segmentation logic of `tui::widgets::parse_message_segments`:
/// an unmatched opening `<think>` causes everything after it to be treated as
/// reasoning (a truncated/malformed stream), and the remaining real text is
/// trimmed and collapsed so a removed leading/trailing block leaves no blank
/// lines behind.
fn strip_think_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut in_think = false;
    while !rest.is_empty() {
        let tag = if in_think { "</think>" } else { "<think>" };
        match rest.find(tag) {
            Some(idx) => {
                if !in_think {
                    out.push_str(&rest[..idx]);
                }
                rest = &rest[idx + tag.len()..];
                in_think = !in_think;
            }
            None => {
                if !in_think {
                    out.push_str(rest);
                }
                break;
            }
        }
    }
    // A removed block often leaves blank lines where it sat between text; trim
    // each line's trailing space, drop empty lines, then rejoin. This collapses
    // the gap without flattening intentional spacing elsewhere.
    out.lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_plugin_sdk::channel::{ChannelCapabilities, ChannelError, Peer, PeerKind, Sender};
    use legion_runtime::{RunEvent, RunRequest, RunStream, RuntimeError};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct RecordingProvider {
        tx: mpsc::UnboundedSender<OutboundMessage>,
        stopped: AtomicBool,
    }

    #[async_trait]
    impl legion_plugin_sdk::channel::ChannelProvider for RecordingProvider {
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

    fn inbound(
        channel: &str,
        peer_kind: PeerKind,
        peer_id: &str,
        sender_id: &str,
    ) -> InboundMessage {
        InboundMessage {
            channel: channel.into(),
            account_id: "default".into(),
            peer: Peer {
                kind: peer_kind,
                id: peer_id.into(),
                name: None,
                thread_id: None,
            },
            sender: Sender {
                id: sender_id.into(),
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
        }
    }

    #[test]
    fn should_build_session_key_for_dm() {
        let msg = inbound("telegram", PeerKind::Direct, "123", "123");
        assert_eq!(
            channel_session_key("main", "main", &msg),
            "agent:main:main:telegram:default:direct:123"
        );
    }

    #[test]
    fn should_build_session_key_for_group() {
        let msg = inbound("telegram", PeerKind::Group, "g1", "u1");
        assert_eq!(
            channel_session_key("work", "main", &msg),
            "agent:work:main:telegram:default:group:g1"
        );
    }

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
    impl legion_plugin_sdk::channel::ChannelProvider for TypingRecordingProvider {
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

    fn open_dm_config(channel: &str) -> Config {
        let json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "channels": {{ "{channel}": {{ "access": {{ "dmPolicy": "open" }} }} }}
            }}"#
        );
        Config::from_json(&json).expect("config should parse")
    }

    /// Registry with the recording channel wired to an outbound receiver.
    fn recording_registry() -> (PluginRegistry, mpsc::UnboundedReceiver<OutboundMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let provider: Arc<dyn legion_plugin_sdk::channel::ChannelProvider> =
            Arc::new(RecordingProvider {
                tx,
                stopped: AtomicBool::new(false),
            });
        let mut registry = PluginRegistry::new();
        registry
            .register_channel("recording", provider)
            .expect("channel should register");
        (registry, rx)
    }

    fn test_session_store() -> (tempfile::TempDir, Arc<SessionStore>) {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let store = Arc::new(SessionStore::new(dir.path()));
        (dir, store)
    }

    #[tokio::test(start_paused = true)]
    async fn route_triggers_typing_and_reaction_when_capabilities_advertise() {
        let provider = Arc::new(TypingRecordingProvider::new());
        let mut registry = PluginRegistry::new();
        registry
            .register_channel("typingrec", provider.clone())
            .expect("channel should register");

        let (_dir, store) = test_session_store();
        let runtime: Arc<dyn Harness> = Arc::new(StubHarness { events: vec![] });
        route_inbound_to_runtime(
            runtime,
            open_dm_config("typingrec"),
            Arc::new(Router::default()),
            store,
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
        let (registry, mut rx) = recording_registry();
        let (_dir, store) = test_session_store();

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
            Arc::new(Router::default()),
            store,
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

    #[tokio::test]
    async fn route_persists_transcript_to_session_store() {
        let (registry, mut rx) = recording_registry();
        let (_dir, store) = test_session_store();
        let session_key = "agent:main:main:recording:default:direct:u1".to_string();

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
            Arc::new(Router::default()),
            store.clone(),
            Arc::new(registry),
            None,
            None,
            InboundMessage::direct("recording", "default", "u1", "hello"),
        )
        .await;

        // Wait for the reply: it is sent after the transcript is persisted.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("reply should be sent promptly");

        let history = store.load_for_resume(&session_key).await;
        let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["hello", "hi there"],
            "transcript should contain the user message and the assistant reply"
        );
    }

    #[tokio::test]
    async fn route_denies_dm_not_in_allowlist() {
        let harness = counting_harness();
        let (registry, mut rx) = recording_registry();
        let (_dir, store) = test_session_store();
        // No `access` block: dmPolicy defaults to allowlist with an empty
        // allowlist, so a stranger's DM must be denied before routing.
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#)
            .expect("config should parse");

        route_inbound_to_runtime(
            harness.clone(),
            config,
            Arc::new(Router::default()),
            store,
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
        let (_dir, store) = test_session_store();
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
            Arc::new(Router::default()),
            store,
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
        let guard = Arc::new(BotLoopGuard::new(std::time::Duration::from_secs(3600), 2));

        // Baseline: an allowed DM reaches the runtime.
        let (registry, _rx) = recording_registry();
        let (_dir, store) = test_session_store();
        route_inbound_to_runtime(
            harness.clone(),
            open_dm_config("recording"),
            Arc::new(Router::default()),
            store,
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
        let (_dir, store) = test_session_store();
        route_inbound_to_runtime(
            harness.clone(),
            open_dm_config("recording"),
            Arc::new(Router::default()),
            store,
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

    #[test]
    fn strip_think_blocks_removes_reasoning_keeps_reply() {
        let text = "<think>let me think about this</think>\nHello!";
        assert_eq!(strip_think_blocks(text), "Hello!");
    }

    #[test]
    fn strip_think_blocks_keeps_text_around_blocks() {
        let text = "Before.\n<think>reasoning</think>\nAfter.";
        assert_eq!(strip_think_blocks(text), "Before.\nAfter.");
    }

    #[test]
    fn strip_think_blocks_handles_multiple_blocks() {
        let text = "<think>a</think>one<think>b</think>two";
        assert_eq!(strip_think_blocks(text), "onetwo");
    }

    #[test]
    fn strip_think_blocks_treats_unclosed_as_rest_reasoning() {
        // A truncated stream with an unmatched <think> drops the trailing run,
        // matching the TUI's segmentation behavior.
        let text = "visible<think>still reasoning and never closes";
        assert_eq!(strip_think_blocks(text), "visible");
    }

    #[test]
    fn strip_think_blocks_passes_through_plain_text() {
        assert_eq!(
            strip_think_blocks("just a normal reply"),
            "just a normal reply"
        );
        // An opening `<think>` with no closing tag is treated as reasoning to
        // the end, mirroring the TUI's segmentation (a malformed/truncated
        // stream). This is consistent, documented behavior — not a literal.
        assert_eq!(strip_think_blocks("visible text"), "visible text");
    }

    #[test]
    fn strip_think_blocks_reasoning_only_yields_empty() {
        assert_eq!(strip_think_blocks("<think>all reasoning</think>"), "");
        assert_eq!(
            strip_think_blocks("<think>all reasoning</think>\n   \n"),
            ""
        );
    }
}
