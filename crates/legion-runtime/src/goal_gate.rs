//! Turn-end gate that keeps the run going while the session goal is active.
//!
//! The gate is goal-driven: it inspects the session goal persisted by the
//! `/goal` command or the `update_goal` tool. While the goal is active, the
//! agent loop appends a system reminder instead of ending the turn ("goal
//! turns"). There is deliberately no turn cap: the goal itself is the
//! limiter. The run ends when the goal becomes inactive — completed, paused,
//! or blocked by the model (`update_goal`), or stopped by the operator
//! (`/goal pause|clear`).

use crate::goal::{Goal, GoalStore};

/// Result of evaluating the turn-end goal gate.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalGateResult {
    /// No active goal (or the gate is disabled); the turn may end.
    Pass,
    /// The goal is still active; continue with the given reminder.
    Continue { reminder: String },
}

/// Turn-end gate tied to the persisted session goal.
#[derive(Debug, Clone)]
pub struct GoalGate {
    store: GoalStore,
    session_key: String,
    enabled: bool,
}

impl GoalGate {
    /// A gate that always passes (goals disabled, or a sub-agent run).
    pub fn disabled() -> Self {
        Self {
            store: GoalStore::default(),
            session_key: String::new(),
            enabled: false,
        }
    }

    pub fn new(store: GoalStore, session_key: impl Into<String>) -> Self {
        Self {
            store,
            session_key: session_key.into(),
            enabled: true,
        }
    }

    /// Load the persisted goal once (at run start), returning it only when
    /// active. Errors are logged and treated as "no goal" so a broken store
    /// never traps a run.
    pub async fn load_active(&self) -> Option<Goal> {
        if !self.enabled {
            return None;
        }
        self.load_active_inner().await
    }

    async fn load_active_inner(&self) -> Option<Goal> {
        match self.store.load(&self.session_key).await {
            Ok(Some(goal)) if goal.status.is_active() => Some(goal),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(error = %err, "goal gate: failed to load session goal");
                None
            }
        }
    }

    /// Evaluate the gate at turn end. While the goal is active the gate
    /// always continues; the goal itself is the only limiter.
    pub async fn check(&self) -> GoalGateResult {
        if !self.enabled {
            return GoalGateResult::Pass;
        }
        let Some(goal) = self.load_active_inner().await else {
            return GoalGateResult::Pass;
        };
        GoalGateResult::Continue {
            reminder: goal_turn_reminder(&goal),
        }
    }
}

/// Render the system reminder that keeps the model working on the goal.
pub fn goal_turn_reminder(goal: &Goal) -> String {
    format!(
        "The session goal is still active — {}. Continue working toward it. \
         When it is achieved, call update_goal with status=\"complete\"; if \
         genuinely stuck, status=\"blocked\"; if you need user input, \
         status=\"paused\". The turn will not end while the goal is active.",
        goal.objective
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::GoalStatus;

    const KEY: &str = "agent:main:dm:cli:default:direct:peer-1";

    fn gate(dir: &std::path::Path) -> GoalGate {
        GoalGate::new(GoalStore::new(dir), KEY)
    }

    async fn save(dir: &std::path::Path, goal: &Goal) {
        GoalStore::new(dir).save(KEY, goal).await.unwrap();
    }

    #[tokio::test]
    async fn disabled_gate_always_passes() {
        let gate = GoalGate::disabled();
        assert_eq!(gate.check().await, GoalGateResult::Pass);
        assert!(gate.load_active().await.is_none());
    }

    #[tokio::test]
    async fn passes_when_no_goal() {
        let dir = tempfile::tempdir().unwrap();
        let gate = gate(dir.path());
        assert_eq!(gate.check().await, GoalGateResult::Pass);
        assert!(gate.load_active().await.is_none());
    }

    #[tokio::test]
    async fn passes_when_goal_not_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut goal = Goal::new("fix bug");
        goal.set_status(GoalStatus::Paused, None);
        save(dir.path(), &goal).await;

        let gate = gate(dir.path());
        assert_eq!(gate.check().await, GoalGateResult::Pass);
        assert!(gate.load_active().await.is_none());
    }

    #[tokio::test]
    async fn continues_indefinitely_while_active() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &Goal::new("fix bug")).await;

        let gate = gate(dir.path());
        // No turn cap: the gate keeps continuing as long as the goal is
        // active — far beyond any fixed budget.
        for _ in 0..100 {
            match gate.check().await {
                GoalGateResult::Continue { reminder } => {
                    assert!(reminder.contains("fix bug"));
                    assert!(reminder.contains("update_goal"));
                }
                other => panic!("expected Continue, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn stops_continuing_once_goal_inactive() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &Goal::new("fix bug")).await;

        let gate = gate(dir.path());
        assert!(matches!(
            gate.check().await,
            GoalGateResult::Continue { .. }
        ));

        // The model marks the goal complete (via update_goal); the next
        // turn-end check passes.
        let mut goal = Goal::new("fix bug");
        goal.set_status(GoalStatus::Complete, None);
        save(dir.path(), &goal).await;
        assert_eq!(gate.check().await, GoalGateResult::Pass);
    }

    #[tokio::test]
    async fn load_active_returns_goal_only_when_active() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &Goal::new("fix bug")).await;

        let gate = gate(dir.path());
        let goal = gate.load_active().await.unwrap();
        assert_eq!(goal.objective, "fix bug");
    }
}
