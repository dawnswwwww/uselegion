//! Async approval gate for tool execution.
//!
//! When a tool-permission decider returns `Permission::Prompt`, the runtime
//! uses an [`ApprovalGate`] to ask the originating user (via an
//! [`ApprovalNotifier`]) and await their decision. Unattended sessions
//! (`interactive == false`) and timed-out requests fail closed (denied).
//!
//! Part 1 of the approval-loop gap (`docs/design/gaps/03-shallow/approval-loop.md`):
//! this module provides the gate and notifier abstractions. Wiring it into the
//! agent loop and the channel side lands in Part 2.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, oneshot};

/// Describes a tool invocation awaiting approval.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub agent_id: String,
    pub session_key: String,
    /// Whether a human can receive and answer the prompt. Unattended runs
    /// (cron, heartbeat) set this to `false` and fail closed.
    pub interactive: bool,
}

/// Bundle of an approval gate and the interactivity flag for a run, passed
/// into the tool pipeline so `Permission::Prompt` can be resolved by asking
/// the user (or failing closed when unattended).
#[derive(Clone)]
pub struct ApprovalCtx {
    pub gate: Arc<ApprovalGate>,
    pub interactive: bool,
}

/// Notifies the user (typically via the originating channel) that an approval
/// decision is needed for `prompt_id`. The concrete channel wiring lands in
/// Part 2; tests use a capturing notifier.
#[async_trait]
pub trait ApprovalNotifier: Send + Sync {
    async fn notify(&self, req: &ApprovalRequest, prompt_id: &str);
}

/// A notifier that does nothing. Used when no channel-side approval wiring is
/// available; prompts will time out and be denied.
#[derive(Debug)]
pub struct NoOpApprovalNotifier;

#[async_trait]
impl ApprovalNotifier for NoOpApprovalNotifier {
    async fn notify(&self, _req: &ApprovalRequest, _prompt_id: &str) {}
}

/// Shared queue for a single approval session. The runtime's [`ApprovalGate`]
/// registers pending prompts here; the channel/user side resolves them by
/// prompt id.
pub struct ApprovalQueue {
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Register a new prompt and return the receiver that will be fulfilled
    /// when [`ApprovalQueue::resolve`] is called (or dropped on timeout).
    pub async fn register(&self, prompt_id: String) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(prompt_id, tx);
        rx
    }

    /// Resolve a pending prompt. Returns `true` if a waiter was found.
    pub async fn resolve(&self, prompt_id: &str, allow: bool) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(prompt_id) {
            let _ = tx.send(allow);
            true
        } else {
            false
        }
    }
}

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Global registry that maps prompt ids to their originating session queue.
/// The Gateway holds one registry and uses it to route channel-side approval
/// replies back to the runtime gate that is waiting for them.
pub struct ApprovalQueueRegistry {
    queues: Mutex<HashMap<String, Arc<ApprovalQueue>>>,
}

impl ApprovalQueueRegistry {
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
        }
    }

    /// Register a queue for a prompt id. Called by the runtime before a prompt
    /// is sent to the user.
    pub async fn register(&self, prompt_id: String, queue: Arc<ApprovalQueue>) {
        self.queues.lock().await.insert(prompt_id, queue);
    }

    /// Resolve a prompt by id. Returns `true` if the prompt was known.
    pub async fn resolve(&self, prompt_id: &str, allow: bool) -> bool {
        let queue = self.queues.lock().await.remove(prompt_id);
        match queue {
            Some(q) => q.resolve(prompt_id, allow).await,
            None => false,
        }
    }
}

impl Default for ApprovalQueueRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Async approval gate: registers pending prompts, asks via the notifier, and
/// resolves when the user replies (or times out / is unattended).
pub struct ApprovalGate {
    notifier: Arc<dyn ApprovalNotifier>,
    queue: Arc<ApprovalQueue>,
    timeout: Duration,
    counter: AtomicU64,
    /// Tools explicitly denied by the user in this session. Once denied, the
    /// gate will not prompt again for the same tool within the same gate
    /// lifetime (typically one agent run).
    session_denies: Mutex<HashSet<String>>,
    /// Optional global registry used to route channel-side approval replies
    /// back to this gate.
    registry: Option<Arc<ApprovalQueueRegistry>>,
    /// Yolo mode: approve every request immediately without notifying or
    /// waiting for a human. Used by `legion agent --yolo`; hard policy
    /// denies (`Permission::Deny`) are decided upstream and unaffected.
    auto_approve: bool,
}

impl ApprovalGate {
    pub fn new(notifier: Arc<dyn ApprovalNotifier>, timeout: Duration) -> Self {
        Self {
            notifier,
            queue: Arc::new(ApprovalQueue::new()),
            timeout,
            counter: AtomicU64::new(0),
            session_denies: Mutex::new(HashSet::new()),
            registry: None,
            auto_approve: false,
        }
    }

    /// Attach a global registry so that channel-side approval replies can be
    /// routed back to this gate by prompt id.
    pub fn with_registry(mut self, registry: Arc<ApprovalQueueRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Enable or disable yolo mode: when enabled, every approval request is
    /// accepted immediately without notifying the user.
    pub fn with_auto_approve(mut self, enabled: bool) -> Self {
        self.auto_approve = enabled;
        self
    }

    /// Access the underlying queue so the Gateway can register it for
    /// channel-side resolution.
    pub fn queue(&self) -> Arc<ApprovalQueue> {
        self.queue.clone()
    }

    /// Ask for approval and await the user's decision. Returns `true` if the
    /// tool may execute. Unattended requests and timeouts return `false`.
    pub async fn request(&self, req: &ApprovalRequest) -> bool {
        if self.auto_approve {
            tracing::info!(tool = req.tool, "yolo mode: auto-approving tool");
            return true;
        }
        if !req.interactive {
            return false;
        }
        if self.session_denies.lock().await.contains(&req.tool) {
            tracing::info!(
                tool = req.tool,
                "session deny hit; skipping approval prompt"
            );
            return false;
        }
        let prompt_id = self.next_prompt_id();
        if let Some(registry) = &self.registry {
            registry
                .register(prompt_id.clone(), self.queue.clone())
                .await;
        }
        let rx = self.queue.register(prompt_id.clone()).await;
        self.notifier.notify(req, &prompt_id).await;
        // Await the user's decision, failing closed on timeout or a dropped
        // resolver (e.g. the channel disconnected without answering).
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(true)) => true,
            Ok(Ok(false)) => {
                self.session_denies.lock().await.insert(req.tool.clone());
                false
            }
            Ok(Err(_)) => false,
            Err(_) => false,
        }
    }

    /// Resolve a pending prompt (convenience wrapper around the queue).
    pub async fn resolve(&self, prompt_id: &str, allow: bool) {
        self.queue.resolve(prompt_id, allow).await;
    }

    fn next_prompt_id(&self) -> String {
        format!("prompt-{}", self.counter.fetch_add(1, Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    struct CapturingNotifier {
        ids: Mutex<mpsc::UnboundedSender<String>>,
    }

    #[async_trait]
    impl ApprovalNotifier for CapturingNotifier {
        async fn notify(&self, _req: &ApprovalRequest, prompt_id: &str) {
            let _ = self.ids.lock().await.send(prompt_id.to_string());
        }
    }

    fn req(interactive: bool) -> ApprovalRequest {
        ApprovalRequest {
            tool: "exec".into(),
            agent_id: "a1".into(),
            session_key: "agent:a:dm:webchat:u1:peer:p1".into(),
            interactive,
        }
    }

    #[tokio::test]
    async fn request_resolves_true_when_user_approves() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = Arc::new(ApprovalGate::new(notifier, Duration::from_secs(5)));
        let r = req(true);

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.request(&r).await });

        let prompt_id = rx.recv().await.expect("notifier should fire");
        gate.resolve(&prompt_id, true).await;
        assert!(handle.await.unwrap(), "approved request must return true");
    }

    #[tokio::test]
    async fn request_resolves_false_when_user_denies() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = Arc::new(ApprovalGate::new(notifier, Duration::from_secs(5)));
        let r = req(true);

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.request(&r).await });

        let prompt_id = rx.recv().await.unwrap();
        gate.resolve(&prompt_id, false).await;
        assert!(!handle.await.unwrap(), "denied request must return false");
    }

    #[tokio::test]
    async fn request_denies_when_unattended() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = ApprovalGate::new(notifier, Duration::from_secs(5));

        let allowed = gate.request(&req(false)).await;
        assert!(!allowed, "unattended requests must fail closed");
        assert!(
            rx.try_recv().is_err(),
            "notifier must not fire when unattended"
        );
    }

    #[tokio::test]
    async fn auto_approve_gate_accepts_without_notifying() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = ApprovalGate::new(notifier, Duration::from_secs(5)).with_auto_approve(true);

        // Interactive and unattended requests alike are accepted immediately.
        assert!(gate.request(&req(true)).await);
        assert!(
            gate.request(&req(false)).await,
            "yolo mode must not fail closed on unattended runs"
        );
        assert!(
            rx.try_recv().is_err(),
            "notifier must not fire in yolo mode"
        );
    }

    #[tokio::test]
    async fn request_denies_on_timeout() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = ApprovalGate::new(notifier, Duration::from_millis(50));

        let start = std::time::Instant::now();
        let allowed = gate.request(&req(true)).await;
        let elapsed = start.elapsed();

        assert!(!allowed, "timed-out request must fail closed");
        assert!(
            elapsed >= Duration::from_millis(40),
            "should wait for the timeout, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn request_session_deny_skips_second_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = Arc::new(ApprovalGate::new(notifier, Duration::from_secs(5)));

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.request(&req(true)).await });
        let prompt_id = rx.recv().await.expect("first notifier should fire");
        gate.resolve(&prompt_id, false).await;
        assert!(!handle.await.unwrap(), "denied request must return false");

        // A second request for the same tool must be denied without notifying.
        let allowed = gate.request(&req(true)).await;
        assert!(!allowed, "session deny must skip second prompt");
        assert!(
            rx.try_recv().is_err(),
            "notifier must not fire after session deny"
        );
    }

    #[tokio::test]
    async fn registry_resolves_registered_prompt() {
        let registry = ApprovalQueueRegistry::new();
        let queue = Arc::new(ApprovalQueue::new());
        let rx = queue.register("prompt-7".into()).await;
        registry.register("prompt-7".into(), queue).await;

        let resolved = registry.resolve("prompt-7", true).await;
        assert!(resolved);
        assert!(rx.await.unwrap(), "resolved prompt must send allow");
    }

    #[tokio::test]
    async fn registry_resolve_unknown_prompt_returns_false() {
        let registry = ApprovalQueueRegistry::new();
        let resolved = registry.resolve("prompt-missing", true).await;
        assert!(!resolved);
    }

    #[tokio::test]
    async fn request_resolves_via_global_registry() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let registry = Arc::new(ApprovalQueueRegistry::new());
        let gate = Arc::new(
            ApprovalGate::new(notifier, Duration::from_secs(5)).with_registry(registry.clone()),
        );
        let r = req(true);

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.request(&r).await });

        let prompt_id = rx.recv().await.expect("notifier should fire");
        // Channel-side path: the gateway resolves `approve:<id>` replies
        // through the shared registry, never touching the gate directly.
        assert!(
            registry.resolve(&prompt_id, true).await,
            "registry must route the approval to the waiting gate"
        );
        assert!(
            handle.await.expect("request task must not panic"),
            "approved request must return true"
        );
        // The registry consumes the entry, so a replayed reply is a miss.
        assert!(!registry.resolve(&prompt_id, true).await);
    }
}
