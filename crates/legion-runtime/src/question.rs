//! Async question gate for interactive user choices.
//!
//! When the `ask_user` tool is invoked, the runtime uses a [`QuestionGate`] to
//! ask the originating user (via a [`QuestionNotifier`]) and await their
//! answer. Unattended sessions (`interactive == false`) and timed-out requests
//! fail closed (return `None`).
//!
//! The design mirrors [`crate::approval`] but carries structured answers
//! ([`AskUserOutput`]) instead of a boolean.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, oneshot};

/// One option inside an `AskUserQuestion`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskUserOption {
    /// Short display text (1-5 words) shown to the user.
    pub label: String,
    /// Explanation of the option and its implications.
    pub description: String,
    /// Optional preview content (markdown) shown when the option is focused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// A single multiple-choice question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskUserQuestion {
    /// The question text, ending with a question mark.
    pub question: String,
    /// Short chip/label shown alongside the question.
    pub header: String,
    /// Available choices (2-4 options).
    pub options: Vec<AskUserOption>,
    /// If true, the user may select multiple options.
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
}

/// Input to the `ask_user` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskUserInput {
    /// Questions to ask the user (1-4 questions).
    pub questions: Vec<AskUserQuestion>,
}

/// Optional per-question annotations returned with the answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskUserAnnotation {
    /// Preview content of the selected option, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Free-text notes the user added (e.g. via the "Other" input).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Output returned by the `ask_user` tool: the questions that were asked plus
/// the user's answers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskUserOutput {
    pub questions: Vec<AskUserQuestion>,
    /// Mapping from question text to answer label. For multi-select questions
    /// the labels are comma-separated.
    pub answers: HashMap<String, String>,
    /// Optional per-question annotations (selected preview, user notes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, AskUserAnnotation>>,
}

/// Describes a question invocation awaiting an answer.
#[derive(Debug, Clone)]
pub struct QuestionRequest {
    pub tool: String,
    pub agent_id: String,
    pub session_key: String,
    /// Whether a human can receive and answer the prompt. Unattended runs
    /// (cron, heartbeat) set this to `false` and fail closed.
    pub interactive: bool,
}

/// Bundle of a question gate and the interactivity flag for a run, passed into
/// the tool pipeline so `ask_user` can be resolved by asking the user.
#[derive(Clone)]
pub struct QuestionCtx {
    pub gate: Arc<QuestionGate>,
    pub interactive: bool,
}

/// Notifies the user (typically via the originating channel/TUI) that a
/// question is waiting for `prompt_id`.
#[async_trait]
pub trait QuestionNotifier: Send + Sync {
    async fn notify(&self, req: &QuestionRequest, prompt_id: &str, questions: &[AskUserQuestion]);
}

/// A notifier that does nothing. Used when no channel-side question wiring is
/// available; prompts will time out and be denied.
#[derive(Debug)]
pub struct NoOpQuestionNotifier;

#[async_trait]
impl QuestionNotifier for NoOpQuestionNotifier {
    async fn notify(
        &self,
        _req: &QuestionRequest,
        _prompt_id: &str,
        _questions: &[AskUserQuestion],
    ) {
    }
}

/// Shared queue for a single question session. The runtime's [`QuestionGate`]
/// registers pending prompts here; the channel/user side resolves them by
/// prompt id.
pub struct QuestionQueue {
    pending: Mutex<HashMap<String, oneshot::Sender<AskUserOutput>>>,
}

impl QuestionQueue {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Register a new prompt and return the receiver that will be fulfilled
    /// when [`QuestionQueue::resolve`] is called (or dropped on timeout).
    pub async fn register(&self, prompt_id: String) -> oneshot::Receiver<AskUserOutput> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(prompt_id, tx);
        rx
    }

    /// Resolve a pending prompt. Returns `true` if a waiter was found.
    pub async fn resolve(&self, prompt_id: &str, answer: AskUserOutput) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(prompt_id) {
            let _ = tx.send(answer);
            true
        } else {
            false
        }
    }
}

impl Default for QuestionQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Global registry that maps prompt ids to their originating session queue.
/// The Gateway holds one registry and uses it to route channel-side question
/// replies back to the runtime gate that is waiting for them.
pub struct QuestionQueueRegistry {
    queues: Mutex<HashMap<String, Arc<QuestionQueue>>>,
}

impl QuestionQueueRegistry {
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
        }
    }

    /// Register a queue for a prompt id. Called by the runtime before a prompt
    /// is sent to the user.
    pub async fn register(&self, prompt_id: String, queue: Arc<QuestionQueue>) {
        self.queues.lock().await.insert(prompt_id, queue);
    }

    /// Resolve a prompt by id. Returns `true` if the prompt was known.
    pub async fn resolve(&self, prompt_id: &str, answer: AskUserOutput) -> bool {
        let queue = self.queues.lock().await.remove(prompt_id);
        match queue {
            Some(q) => q.resolve(prompt_id, answer).await,
            None => false,
        }
    }
}

impl Default for QuestionQueueRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Async question gate: registers pending prompts, asks via the notifier, and
/// resolves when the user replies (or times out / is unattended).
pub struct QuestionGate {
    notifier: Arc<dyn QuestionNotifier>,
    queue: Arc<QuestionQueue>,
    timeout: Duration,
    counter: AtomicU64,
    /// Optional global registry used to route channel-side question replies
    /// back to this gate.
    registry: Option<Arc<QuestionQueueRegistry>>,
}

impl QuestionGate {
    pub fn new(notifier: Arc<dyn QuestionNotifier>, timeout: Duration) -> Self {
        Self {
            notifier,
            queue: Arc::new(QuestionQueue::new()),
            timeout,
            counter: AtomicU64::new(0),
            registry: None,
        }
    }

    /// Attach a global registry so that channel-side question replies can be
    /// routed back to this gate by prompt id.
    pub fn with_registry(mut self, registry: Arc<QuestionQueueRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Access the underlying queue so the Gateway can register it for
    /// channel-side resolution.
    pub fn queue(&self) -> Arc<QuestionQueue> {
        self.queue.clone()
    }

    /// Ask the user a question and await their answer. Returns `Some(answer)`
    /// on success. Unattended requests and timeouts return `None`.
    pub async fn request(
        &self,
        req: &QuestionRequest,
        questions: &[AskUserQuestion],
    ) -> Option<AskUserOutput> {
        if !req.interactive {
            return None;
        }
        let prompt_id = self.next_prompt_id();
        if let Some(registry) = &self.registry {
            registry
                .register(prompt_id.clone(), self.queue.clone())
                .await;
        }
        let rx = self.queue.register(prompt_id.clone()).await;
        self.notifier.notify(req, &prompt_id, questions).await;
        // Await the user's decision, failing closed on timeout or a dropped
        // resolver (e.g. the channel disconnected without answering).
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(answer)) => Some(answer),
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Resolve a pending prompt (convenience wrapper around the queue).
    pub async fn resolve(&self, prompt_id: &str, answer: AskUserOutput) {
        self.queue.resolve(prompt_id, answer).await;
    }

    fn next_prompt_id(&self) -> String {
        format!("question-{}", self.counter.fetch_add(1, Ordering::SeqCst))
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
    impl QuestionNotifier for CapturingNotifier {
        async fn notify(
            &self,
            _req: &QuestionRequest,
            prompt_id: &str,
            _questions: &[AskUserQuestion],
        ) {
            let _ = self.ids.lock().await.send(prompt_id.to_string());
        }
    }

    fn sample_question() -> AskUserQuestion {
        AskUserQuestion {
            question: "Which color?".into(),
            header: "Color".into(),
            options: vec![
                AskUserOption {
                    label: "Red".into(),
                    description: "Warm".into(),
                    preview: None,
                },
                AskUserOption {
                    label: "Blue".into(),
                    description: "Cool".into(),
                    preview: None,
                },
            ],
            multi_select: false,
        }
    }

    fn req(interactive: bool) -> QuestionRequest {
        QuestionRequest {
            tool: "ask_user".into(),
            agent_id: "a1".into(),
            session_key: "agent:a:dm:webchat:u1:peer:p1".into(),
            interactive,
        }
    }

    fn answer() -> AskUserOutput {
        AskUserOutput {
            questions: vec![sample_question()],
            answers: [("Which color?".into(), "Red".into())].into(),
            annotations: None,
        }
    }

    #[tokio::test]
    async fn request_resolves_with_answer_when_user_answers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = Arc::new(QuestionGate::new(notifier, Duration::from_secs(5)));
        let r = req(true);
        let q = vec![sample_question()];

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.request(&r, &q).await });

        let prompt_id = rx.recv().await.expect("notifier should fire");
        gate.resolve(&prompt_id, answer()).await;
        assert_eq!(
            handle.await.unwrap(),
            Some(answer()),
            "answered request must return the answer"
        );
    }

    #[tokio::test]
    async fn request_returns_none_when_unattended() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = QuestionGate::new(notifier, Duration::from_secs(5));

        let got = gate.request(&req(false), &[sample_question()]).await;
        assert!(got.is_none(), "unattended requests must fail closed");
    }

    #[tokio::test]
    async fn request_returns_none_on_timeout() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let gate = QuestionGate::new(notifier, Duration::from_millis(50));

        let start = std::time::Instant::now();
        let got = gate.request(&req(true), &[sample_question()]).await;
        let elapsed = start.elapsed();

        assert!(got.is_none(), "timed-out request must fail closed");
        assert!(
            elapsed >= Duration::from_millis(40),
            "should wait for the timeout, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn registry_resolves_registered_prompt() {
        let registry = QuestionQueueRegistry::new();
        let queue = Arc::new(QuestionQueue::new());
        let rx = queue.register("question-7".into()).await;
        registry.register("question-7".into(), queue).await;

        let resolved = registry.resolve("question-7", answer()).await;
        assert!(resolved);
        assert_eq!(
            rx.await.unwrap(),
            answer(),
            "resolved prompt must send answer"
        );
    }

    #[tokio::test]
    async fn registry_resolve_unknown_prompt_returns_false() {
        let registry = QuestionQueueRegistry::new();
        let resolved = registry.resolve("question-missing", answer()).await;
        assert!(!resolved);
    }

    #[tokio::test]
    async fn request_resolves_via_global_registry() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingNotifier {
            ids: Mutex::new(tx),
        });
        let registry = Arc::new(QuestionQueueRegistry::new());
        let gate = Arc::new(
            QuestionGate::new(notifier, Duration::from_secs(5)).with_registry(registry.clone()),
        );
        let r = req(true);
        let q = vec![sample_question()];

        let g = gate.clone();
        let handle = tokio::spawn(async move { g.request(&r, &q).await });

        let prompt_id = rx.recv().await.expect("notifier should fire");
        assert!(
            registry.resolve(&prompt_id, answer()).await,
            "registry must route the answer to the waiting gate"
        );
        assert!(
            handle.await.expect("request task must not panic").is_some(),
            "answered request must return Some"
        );
        // The registry consumes the entry, so a replayed reply is a miss.
        assert!(!registry.resolve(&prompt_id, answer()).await);
    }
}
