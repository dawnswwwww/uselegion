use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::subagent::{SubagentKind, SubagentRequest, SubagentSpawner};
use legion_provider::types::ChatMessage;

/// Default maximum number of named teammates per swarm.
pub const DEFAULT_MAX_TEAMMATES: usize = 8;
/// Default per-teammate mailbox capacity.
pub const DEFAULT_MAILBOX_CAPACITY: usize = 16;
/// Number of conversation messages retained per teammate across turns.
const MAX_HISTORY_MESSAGES: usize = 40;
/// Characters of a turn result kept for status reporting.
const LAST_RESULT_CHARS: usize = 500;

/// A message addressed to a teammate's mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmMessage {
    pub from: String,
    pub text: String,
}

/// Lifecycle status of a teammate. Turn failures are recorded in
/// `last_result`, not modelled as a separate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateStatus {
    Running,
    Idle,
}

/// Point-in-time snapshot of a teammate, returned by [`SwarmManager::status`].
#[derive(Debug, Clone)]
pub struct TeammateInfo {
    pub name: String,
    pub agent_type: String,
    pub status: TeammateStatus,
    pub turns: u32,
    pub mailbox_depth: usize,
    pub last_result: Option<String>,
}

/// Errors returned by swarm operations.
#[derive(Debug, Error)]
pub enum SwarmError {
    #[error("invalid teammate name '{0}' (expected ^[A-Za-z0-9._-]{{1,32}}$)")]
    InvalidName(String),
    #[error("teammate '{0}' already exists")]
    AlreadyExists(String),
    #[error("unknown teammate '{0}'")]
    UnknownTeammate(String),
    #[error("teammate '{name}' mailbox is full (capacity {capacity})")]
    MailboxFull { name: String, capacity: usize },
    #[error("too many teammates (max {max})")]
    TooManyTeammates { max: usize },
}

/// Internal per-teammate state.
struct Teammate {
    agent_type: String,
    parent_agent_id: String,
    parent_depth: u8,
    allowed_tools: Option<Vec<String>>,
    status: TeammateStatus,
    mailbox: VecDeque<SwarmMessage>,
    history: Vec<ChatMessage>,
    last_result: Option<String>,
    turns: u32,
}

#[derive(Default)]
struct SwarmState {
    teammates: HashMap<String, Teammate>,
}

/// In-process swarm manager (multi-agent Phase D): named persistent teammates
/// driven by mailboxes. Every teammate turn is executed through the shared
/// [`SubagentSpawner`], so concurrency limits, timeouts, depth guards, and
/// sidechain transcripts come for free.
///
/// The manager is wired late by the gateway (same pattern as the spawner and
/// messenger) because it needs the fully-built `RuntimeSubagentSpawner`.
pub struct SwarmManager {
    spawner: Arc<dyn SubagentSpawner>,
    inner: Mutex<SwarmState>,
    max_teammates: usize,
    mailbox_capacity: usize,
}

impl SwarmManager {
    /// Create a manager with the default teammate and mailbox limits.
    pub fn new(spawner: Arc<dyn SubagentSpawner>) -> Self {
        Self::with_limits(spawner, DEFAULT_MAX_TEAMMATES, DEFAULT_MAILBOX_CAPACITY)
    }

    /// Create a manager with explicit limits (used by tests).
    pub fn with_limits(
        spawner: Arc<dyn SubagentSpawner>,
        max_teammates: usize,
        mailbox_capacity: usize,
    ) -> Self {
        Self {
            spawner,
            inner: Mutex::new(SwarmState::default()),
            max_teammates,
            mailbox_capacity,
        }
    }

    /// Spawn a named teammate and start its first turn in the background.
    ///
    /// The state lock is never held across an await: registration completes
    /// under the lock, then the supervisor task is spawned.
    pub fn spawn_teammate(
        self: &Arc<Self>,
        name: &str,
        agent_type: &str,
        prompt: &str,
        parent_agent_id: &str,
        parent_depth: u8,
        allowed_tools: Option<Vec<String>>,
    ) -> Result<(), SwarmError> {
        validate_name(name)?;
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state.teammates.contains_key(name) {
                return Err(SwarmError::AlreadyExists(name.to_string()));
            }
            if state.teammates.len() >= self.max_teammates {
                return Err(SwarmError::TooManyTeammates {
                    max: self.max_teammates,
                });
            }
            state.teammates.insert(
                name.to_string(),
                Teammate {
                    agent_type: agent_type.to_string(),
                    parent_agent_id: parent_agent_id.to_string(),
                    parent_depth,
                    allowed_tools,
                    status: TeammateStatus::Running,
                    mailbox: VecDeque::new(),
                    history: Vec::new(),
                    last_result: None,
                    turns: 0,
                },
            );
        }
        tokio::spawn(supervise(
            Arc::clone(self),
            name.to_string(),
            Some(prompt.to_string()),
        ));
        Ok(())
    }

    /// Queue a message in a teammate's mailbox, waking it when it is idle.
    pub fn send(self: &Arc<Self>, from: &str, to: &str, text: &str) -> Result<(), SwarmError> {
        let wake = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let tm = state
                .teammates
                .get_mut(to)
                .ok_or_else(|| SwarmError::UnknownTeammate(to.to_string()))?;
            if tm.mailbox.len() >= self.mailbox_capacity {
                return Err(SwarmError::MailboxFull {
                    name: to.to_string(),
                    capacity: self.mailbox_capacity,
                });
            }
            tm.mailbox.push_back(SwarmMessage {
                from: from.to_string(),
                text: text.to_string(),
            });
            if tm.status == TeammateStatus::Idle {
                // Wake under the same lock so the supervisor's drain/Idle
                // check can never observe a stale Running state.
                tm.status = TeammateStatus::Running;
                true
            } else {
                false
            }
        };
        if wake {
            tokio::spawn(supervise(Arc::clone(self), to.to_string(), None));
        }
        Ok(())
    }

    /// Snapshot all teammates, sorted by name.
    pub fn status(&self) -> Vec<TeammateInfo> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut infos: Vec<TeammateInfo> = state
            .teammates
            .iter()
            .map(|(name, tm)| TeammateInfo {
                name: name.clone(),
                agent_type: tm.agent_type.clone(),
                status: tm.status,
                turns: tm.turns,
                mailbox_depth: tm.mailbox.len(),
                last_result: tm.last_result.clone(),
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Number of retained history messages for a teammate (diagnostic aid).
    pub fn history_len(&self, name: &str) -> Option<usize> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.teammates.get(name).map(|tm| tm.history.len())
    }
}

/// Validate a teammate name against `^[A-Za-z0-9._-]{1,32}$`.
fn validate_name(name: &str) -> Result<(), SwarmError> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(SwarmError::InvalidName(name.to_string()))
    }
}

/// Render drained mailbox messages into a single turn prompt.
fn render_mailbox(messages: VecDeque<SwarmMessage>) -> String {
    let mut out = format!("[swarm mailbox: {} message(s)]", messages.len());
    for msg in messages {
        out.push_str(&format!("\nfrom {}: {}", msg.from, msg.text));
    }
    out
}

/// Truncate to at most `max` chars (char-safe) for status reporting.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

/// Drive a teammate through its turns until its mailbox is empty.
///
/// The mailbox drain and the transition to `Idle` always happen under the
/// same lock acquisition, so a concurrent `send` either lands before the
/// drain (picked up by this loop) or observes `Idle` and wakes a fresh
/// supervisor. No message can be stranded.
async fn supervise(manager: Arc<SwarmManager>, name: String, initial: Option<String>) {
    let mut pending_prompt = initial;
    loop {
        // Resolve this turn's prompt: the initial prompt, or a drained
        // mailbox. An empty mailbox means the teammate goes idle.
        let prompt = match pending_prompt.take() {
            Some(p) => p,
            None => {
                let next = {
                    let mut state = manager.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(tm) = state.teammates.get_mut(&name) else {
                        return;
                    };
                    if tm.mailbox.is_empty() {
                        tm.status = TeammateStatus::Idle;
                        None
                    } else {
                        Some(render_mailbox(std::mem::take(&mut tm.mailbox)))
                    }
                };
                match next {
                    Some(p) => p,
                    None => return,
                }
            }
        };

        // Snapshot the teammate's configuration and history for this turn.
        let (agent_type, parent_agent_id, parent_depth, allowed_tools, history) = {
            let state = manager.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(tm) = state.teammates.get(&name) else {
                return;
            };
            (
                tm.agent_type.clone(),
                tm.parent_agent_id.clone(),
                tm.parent_depth,
                tm.allowed_tools.clone(),
                tm.history.clone(),
            )
        };

        let req = SubagentRequest {
            kind: SubagentKind::Typed(agent_type),
            prompt: prompt.clone(),
            model: None,
            allowed_tools,
            parent_agent_id,
            parent_depth,
            system_prompt: None,
            history,
            max_iterations: None,
            timeout: None,
        };

        let outcome = match manager.spawner.spawn(req).await {
            Ok(handle) => handle.join().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };

        let result_text = match outcome {
            Ok(result) => result.text,
            Err(err) => {
                let mut state = manager.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(tm) = state.teammates.get_mut(&name) {
                    tm.last_result = Some(format!("turn failed: {err}"));
                    tm.status = TeammateStatus::Idle;
                }
                return;
            }
        };

        // Record the turn, then drain the mailbox under the same lock to
        // decide whether to continue (see the no-stranding invariant above).
        let next_prompt = {
            let mut state = manager.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(tm) = state.teammates.get_mut(&name) else {
                return;
            };
            tm.history.push(ChatMessage::user(prompt));
            tm.history.push(ChatMessage::assistant(&result_text));
            if tm.history.len() > MAX_HISTORY_MESSAGES {
                let overflow = tm.history.len() - MAX_HISTORY_MESSAGES;
                tm.history.drain(..overflow);
            }
            tm.last_result = Some(truncate_chars(&result_text, LAST_RESULT_CHARS));
            tm.turns += 1;
            if tm.mailbox.is_empty() {
                tm.status = TeammateStatus::Idle;
                None
            } else {
                Some(render_mailbox(std::mem::take(&mut tm.mailbox)))
            }
        };

        match next_prompt {
            Some(p) => pending_prompt = Some(p),
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::{SubagentError, SubagentHandle, SubagentResult, SubagentStatus};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Spawner that records every request and completes immediately with a
    /// canned reply.
    struct RecordingSpawner {
        requests: Mutex<Vec<(String, Vec<ChatMessage>)>>,
    }

    impl RecordingSpawner {
        fn new() -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
            }
        }

        fn recorded(&self) -> Vec<(String, Vec<ChatMessage>)> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl SubagentSpawner for RecordingSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<SubagentHandle, SubagentError> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((req.prompt.clone(), req.history.clone()));
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(SubagentResult {
                handle_id: "h".to_string(),
                text: "canned-reply".to_string(),
                tool_call_count: 0,
                transcript_path: None,
                status: SubagentStatus::Completed,
            });
            Ok(SubagentHandle::from_receiver("h".to_string(), rx))
        }
    }

    /// Spawner whose first turn blocks on a notify gate; later turns complete
    /// immediately. Used to keep a teammate in `Running` state deterministically.
    struct GatedSpawner {
        gate: Arc<tokio::sync::Notify>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl SubagentSpawner for GatedSpawner {
        async fn spawn(&self, _req: SubagentRequest) -> Result<SubagentHandle, SubagentError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                if n == 0 {
                    gate.notified().await;
                }
                let _ = tx.send(SubagentResult {
                    handle_id: "h".to_string(),
                    text: "gated-reply".to_string(),
                    tool_call_count: 0,
                    transcript_path: None,
                    status: SubagentStatus::Completed,
                });
            });
            Ok(SubagentHandle::from_receiver("h".to_string(), rx))
        }
    }

    fn manager(spawner: Arc<dyn SubagentSpawner>) -> Arc<SwarmManager> {
        Arc::new(SwarmManager::new(spawner))
    }

    fn info(mgr: &SwarmManager, name: &str) -> TeammateInfo {
        mgr.status()
            .into_iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("teammate {name} missing"))
    }

    async fn wait_for(mgr: &SwarmManager, name: &str, pred: impl Fn(&TeammateInfo) -> bool) {
        for _ in 0..200 {
            let i = info(mgr, name);
            if pred(&i) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "condition not met for teammate {name}: {:?}",
            info(mgr, name)
        );
    }

    #[tokio::test]
    async fn spawn_runs_turn_and_goes_idle() {
        let spawner = Arc::new(RecordingSpawner::new());
        let mgr = manager(spawner);
        mgr.spawn_teammate("worker", "main", "first task", "main", 0, None)
            .expect("spawn accepted");

        wait_for(&mgr, "worker", |i| i.status == TeammateStatus::Idle).await;
        let i = info(&mgr, "worker");
        assert_eq!(i.turns, 1);
        assert_eq!(
            i.last_result.as_deref(),
            Some("canned-reply"),
            "last_result should hold the turn reply"
        );
        assert_eq!(mgr.history_len("worker"), Some(2));
    }

    #[tokio::test]
    async fn mailbox_message_wakes_idle_teammate() {
        let spawner = Arc::new(RecordingSpawner::new());
        let mgr = manager(spawner.clone());
        mgr.spawn_teammate("worker", "main", "first task", "main", 0, None)
            .expect("spawn accepted");
        wait_for(&mgr, "worker", |i| i.status == TeammateStatus::Idle).await;

        mgr.send("leader", "worker", "follow-up question")
            .expect("send accepted");
        wait_for(&mgr, "worker", |i| {
            i.turns == 2 && i.status == TeammateStatus::Idle
        })
        .await;

        let recorded = spawner.recorded();
        assert_eq!(recorded.len(), 2);
        assert!(
            recorded[1]
                .0
                .contains("[swarm mailbox: 1 message(s)]\nfrom leader: follow-up question"),
            "second turn prompt must render the mailbox, got {:?}",
            recorded[1].0
        );
    }

    #[tokio::test]
    async fn send_to_running_teammate_queues_without_extra_wake() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let spawner = Arc::new(GatedSpawner {
            gate: gate.clone(),
            calls: AtomicUsize::new(0),
        });
        let mgr = manager(spawner.clone());
        mgr.spawn_teammate("worker", "main", "slow task", "main", 0, None)
            .expect("spawn accepted");
        // Wait until the first turn has actually started (blocked on the
        // gate): the status is already Running at registration, but the
        // supervisor task may not have been scheduled yet.
        for _ in 0..200 {
            if spawner.calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(info(&mgr, "worker").status, TeammateStatus::Running);

        // The first turn is blocked on the gate: the message must queue
        // without spawning a second supervisor.
        mgr.send("leader", "worker", "queued note")
            .expect("send accepted");
        assert_eq!(info(&mgr, "worker").mailbox_depth, 1);
        assert_eq!(spawner.calls.load(Ordering::SeqCst), 1);

        // Release the first turn; the supervisor must drain the mailbox and
        // run exactly one more turn.
        gate.notify_one();
        wait_for(&mgr, "worker", |i| {
            i.turns == 2 && i.status == TeammateStatus::Idle
        })
        .await;
        assert_eq!(spawner.calls.load(Ordering::SeqCst), 2);
        assert_eq!(info(&mgr, "worker").mailbox_depth, 0);
    }

    #[tokio::test]
    async fn duplicate_name_rejected() {
        let mgr = manager(Arc::new(RecordingSpawner::new()));
        mgr.spawn_teammate("worker", "main", "a", "main", 0, None)
            .expect("first spawn accepted");
        let err = mgr
            .spawn_teammate("worker", "main", "b", "main", 0, None)
            .expect_err("duplicate name must fail");
        assert!(matches!(err, SwarmError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn invalid_names_rejected() {
        let mgr = manager(Arc::new(RecordingSpawner::new()));
        for name in ["", "with space", "bad/slash", &"x".repeat(33)] {
            let err = mgr
                .spawn_teammate(name, "main", "a", "main", 0, None)
                .expect_err("invalid name must fail");
            assert!(
                matches!(err, SwarmError::InvalidName(_)),
                "name {name:?} should be invalid"
            );
        }
        // Boundary cases that must pass.
        mgr.spawn_teammate("a.B_c-1", "main", "ok", "main", 0, None)
            .expect("valid charset");
        mgr.spawn_teammate(&"y".repeat(32), "main", "ok", "main", 0, None)
            .expect("32 chars is allowed");
    }

    #[tokio::test]
    async fn too_many_teammates_rejected() {
        let spawner = Arc::new(RecordingSpawner::new());
        let mgr = Arc::new(SwarmManager::with_limits(spawner, 1, 16));
        mgr.spawn_teammate("one", "main", "a", "main", 0, None)
            .expect("first spawn accepted");
        let err = mgr
            .spawn_teammate("two", "main", "b", "main", 0, None)
            .expect_err("second teammate exceeds the limit");
        assert!(matches!(err, SwarmError::TooManyTeammates { max: 1 }));
    }

    #[tokio::test]
    async fn full_mailbox_rejected() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let spawner = Arc::new(GatedSpawner {
            gate: gate.clone(),
            calls: AtomicUsize::new(0),
        });
        let mgr = Arc::new(SwarmManager::with_limits(spawner, 8, 1));
        mgr.spawn_teammate("worker", "main", "slow task", "main", 0, None)
            .expect("spawn accepted");
        wait_for(&mgr, "worker", |i| i.status == TeammateStatus::Running).await;

        mgr.send("leader", "worker", "first").expect("first fits");
        let err = mgr
            .send("leader", "worker", "second")
            .expect_err("capacity 1 must reject the second message");
        assert!(matches!(
            err,
            SwarmError::MailboxFull {
                name,
                capacity: 1
            } if name == "worker"
        ));

        // Let the teammate finish cleanly.
        gate.notify_one();
        wait_for(&mgr, "worker", |i| i.status == TeammateStatus::Idle).await;
    }

    #[tokio::test]
    async fn unknown_teammate_rejected() {
        let mgr = manager(Arc::new(RecordingSpawner::new()));
        let err = mgr
            .send("leader", "ghost", "hi")
            .expect_err("unknown teammate must fail");
        assert!(matches!(err, SwarmError::UnknownTeammate(_)));
    }

    /// Spawner that fails every spawn with a canned error.
    struct FailingSpawner {
        error: String,
    }

    #[async_trait]
    impl SubagentSpawner for FailingSpawner {
        async fn spawn(&self, _req: SubagentRequest) -> Result<SubagentHandle, SubagentError> {
            Err(SubagentError::Validation(self.error.clone()))
        }
    }

    #[tokio::test]
    async fn failed_turn_records_error_and_goes_idle() {
        let spawner = Arc::new(FailingSpawner {
            error: "boom-town".to_string(),
        });
        let mgr = manager(spawner);
        mgr.spawn_teammate("worker", "main", "first task", "main", 0, None)
            .expect("spawn accepted");

        wait_for(&mgr, "worker", |i| i.status == TeammateStatus::Idle).await;
        let i = info(&mgr, "worker");
        assert_eq!(i.turns, 0, "a failed turn is not counted");
        let last = i.last_result.expect("failure recorded in last_result");
        assert!(
            last.contains("boom-town"),
            "last_result must carry the error text, got {last:?}"
        );
    }

    #[tokio::test]
    async fn history_truncates_to_max_messages() {
        let spawner = Arc::new(RecordingSpawner::new());
        let mgr = manager(spawner.clone());
        mgr.spawn_teammate("worker", "main", "first task", "main", 0, None)
            .expect("spawn accepted");
        // 1 (initial) + 21 follow-ups = 22 turns; 2 messages per turn would
        // grow the history to 44 without the MAX_HISTORY_MESSAGES cap.
        for n in 1..=21u32 {
            wait_for(&mgr, "worker", |i| {
                i.turns == n && i.status == TeammateStatus::Idle
            })
            .await;
            mgr.send("leader", "worker", &format!("follow-up {n}"))
                .expect("send accepted");
        }
        wait_for(&mgr, "worker", |i| {
            i.turns == 22 && i.status == TeammateStatus::Idle
        })
        .await;

        assert_eq!(
            mgr.history_len("worker"),
            Some(MAX_HISTORY_MESSAGES),
            "history is capped at MAX_HISTORY_MESSAGES"
        );
        let recorded = spawner.recorded();
        let last_history = &recorded.last().expect("turns recorded").1;
        assert_eq!(last_history.len(), MAX_HISTORY_MESSAGES);
        assert!(
            last_history.iter().all(|m| m.content != "first task"),
            "the oldest turns are dropped from the front"
        );
    }

    #[tokio::test]
    async fn history_carries_prior_turns() {
        let spawner = Arc::new(RecordingSpawner::new());
        let mgr = manager(spawner.clone());
        mgr.spawn_teammate("worker", "main", "first task", "main", 0, None)
            .expect("spawn accepted");
        wait_for(&mgr, "worker", |i| i.status == TeammateStatus::Idle).await;

        mgr.send("leader", "worker", "second task")
            .expect("send accepted");
        wait_for(&mgr, "worker", |i| {
            i.turns == 2 && i.status == TeammateStatus::Idle
        })
        .await;

        let recorded = spawner.recorded();
        assert_eq!(recorded.len(), 2);
        let history = &recorded[1].1;
        assert!(
            history.len() >= 2,
            "second turn must carry the first turn's history, got {history:?}"
        );
        assert_eq!(history[0].content, "first task");
        assert_eq!(history[1].content, "canned-reply");
    }

    #[test]
    fn render_mailbox_format_is_stable() {
        let mut msgs = VecDeque::new();
        msgs.push_back(SwarmMessage {
            from: "a".into(),
            text: "one".into(),
        });
        msgs.push_back(SwarmMessage {
            from: "b".into(),
            text: "two".into(),
        });
        assert_eq!(
            render_mailbox(msgs),
            "[swarm mailbox: 2 message(s)]\nfrom a: one\nfrom b: two"
        );
    }
}
