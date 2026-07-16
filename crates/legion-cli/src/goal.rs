//! Session goals: durable per-session objectives managed via `/goal`.
//!
//! Goals are persisted next to session transcripts so they survive process
//! restarts and move with the session key:
//!   `<base_dir>/agents/<agent_id>/goals/<peer_id>.json`
//!
//! Only one goal can exist per session at a time. The active goal is injected
//! as a compact user-role context line on every turn.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Lifecycle status of a session goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The session is pursuing the goal.
    Active,
    /// The operator paused the goal.
    Paused,
    /// The agent or operator reported a genuine blocker.
    Blocked,
    /// The configured token budget was reached.
    BudgetLimited,
    /// Reserved for a future usage-limit stop state.
    UsageLimited,
    /// The goal was achieved (terminal).
    Complete,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::BudgetLimited => "budget_limited",
            GoalStatus::UsageLimited => "usage_limited",
            GoalStatus::Complete => "complete",
        }
    }

    /// True for statuses where the goal is still being pursued.
    pub fn is_active(&self) -> bool {
        matches!(self, GoalStatus::Active)
    }

    /// True for terminal statuses that block resumption.
    pub fn is_terminal(&self) -> bool {
        matches!(self, GoalStatus::Complete)
    }

    /// True for statuses that can be resumed to active.
    pub fn can_resume(&self) -> bool {
        matches!(
            self,
            GoalStatus::Paused
                | GoalStatus::Blocked
                | GoalStatus::BudgetLimited
                | GoalStatus::UsageLimited
        )
    }
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A durable objective attached to the current session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Goal {
    /// Create a new active goal.
    pub fn new(objective: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            objective: objective.into(),
            status: GoalStatus::Active,
            note: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Update the objective text, keeping status and accounting intact.
    pub fn edit(&mut self, objective: impl Into<String>) {
        self.objective = objective.into();
        self.updated_at = Utc::now();
    }

    /// Transition to a new status, recording an optional note.
    pub fn set_status(&mut self, status: GoalStatus, note: Option<String>) {
        self.status = status;
        self.note = note;
        self.updated_at = Utc::now();
    }

    /// Compact context line injected on every user turn while active.
    pub fn context_line(&self) -> String {
        format!(
            "Active goal: {} — advance it or update its status (get_goal/update_goal).",
            self.objective
        )
    }

    /// Human-readable summary shown by `/goal` status.
    pub fn summary(&self) -> String {
        let mut lines = vec![
            "Goal".to_string(),
            format!("Status: {}", self.status),
            format!("Objective: {}", self.objective),
        ];
        if let Some(note) = &self.note {
            lines.push(format!("Note: {note}"));
        }
        lines.join("\n")
    }
}

/// Errors that can occur when interacting with the goal store.
#[derive(Debug, Error)]
pub enum GoalError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid session key")]
    InvalidSessionKey,
}

/// On-disk store for per-session goals.
#[derive(Debug, Clone, Default)]
pub struct GoalStore {
    base_dir: PathBuf,
}

impl GoalStore {
    /// Store rooted at the given directory.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Load the goal for a session key, if one exists.
    pub async fn load(&self, session_key: &str) -> Result<Option<Goal>, GoalError> {
        let path = self.path_for(session_key)?;
        if !path.exists() {
            return Ok(None);
        }
        let text = tokio::fs::read_to_string(&path).await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Persist a goal for a session key.
    pub async fn save(&self, session_key: &str, goal: &Goal) -> Result<(), GoalError> {
        let path = self.path_for(session_key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = tmp_path_for(&path);
        let written = async {
            let text = serde_json::to_string_pretty(goal)?;
            tokio::fs::write(&tmp, text).await?;
            Ok::<(), GoalError>(())
        }
        .await;
        if let Err(err) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(err);
        }
        if let Err(err) = tokio::fs::rename(&tmp, &path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(err.into());
        }
        Ok(())
    }

    /// Remove the goal for a session key.
    pub async fn remove(&self, session_key: &str) -> Result<(), GoalError> {
        let path = self.path_for(session_key)?;
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }

    /// Resolve the on-disk path for a session key.
    fn path_for(&self, session_key: &str) -> Result<PathBuf, GoalError> {
        let parts: Vec<&str> = session_key.split(':').collect();
        if parts.len() != 7 || parts[0] != "agent" {
            return Err(GoalError::InvalidSessionKey);
        }
        let agent_id = parts[1];
        let peer_id = parts[6];
        if !is_safe_segment(agent_id) || !is_safe_segment(peer_id) {
            return Err(GoalError::InvalidSessionKey);
        }
        Ok(self
            .base_dir
            .join("agents")
            .join(agent_id)
            .join("goals")
            .join(format!("{peer_id}.json")))
    }
}

/// Parse a `/goal ...` command into an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAction {
    /// Show the current goal (status with no args is also Show).
    Show,
    /// Create a new goal with the given objective.
    Start(String),
    /// Reword the current goal.
    Edit(String),
    /// Pause the active goal with an optional note.
    Pause(Option<String>),
    /// Resume a paused/blocked/limited goal with an optional note.
    Resume(Option<String>),
    /// Mark the goal complete with an optional note.
    Complete(Option<String>),
    /// Mark the goal blocked with an optional note.
    Block(Option<String>),
    /// Remove the goal from the session.
    Clear,
}

/// Parse the argument string of a `/goal` command.
pub fn parse_goal(args: &str) -> GoalAction {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return GoalAction::Show;
    }

    // Split only on the first whitespace so the rest can be free-form text.
    let (verb, rest) = match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim_start()),
        None => (trimmed, ""),
    };
    let verb_lower = verb.to_lowercase();

    match verb_lower.as_str() {
        "status" => GoalAction::Show,
        "start" | "set" | "create" => GoalAction::Start(rest.to_string()),
        "edit" => GoalAction::Edit(rest.to_string()),
        "pause" => GoalAction::Pause(none_if_empty(rest)),
        "resume" => GoalAction::Resume(none_if_empty(rest)),
        "complete" | "done" => GoalAction::Complete(none_if_empty(rest)),
        "block" | "blocked" => GoalAction::Block(none_if_empty(rest)),
        "clear" => GoalAction::Clear,
        _ => {
            // Any text that is not a recognized action verb creates a new goal.
            GoalAction::Start(trimmed.to_string())
        }
    }
}

fn none_if_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Apply an action to an optional current goal, returning the updated goal
/// (or `None` after a clear) and a human-readable response.
pub fn apply_action(current: Option<Goal>, action: GoalAction) -> (Option<Goal>, String) {
    match action {
        GoalAction::Show => match &current {
            Some(goal) => (current.clone(), goal.summary()),
            None => (
                current,
                "No active goal. Start one with /goal <objective>.".to_string(),
            ),
        },
        GoalAction::Start(objective) => {
            if objective.trim().is_empty() {
                return (
                    current,
                    "Goal error: objective cannot be empty. Usage: /goal <objective>".to_string(),
                );
            }
            match current {
                Some(ref goal) if !goal.status.is_terminal() => (
                    current,
                    "Goal error: goal already exists. Use /goal to inspect it, /goal complete if done, or /goal clear before starting a different objective.".to_string(),
                ),
                _ => {
                    let goal = Goal::new(objective);
                    let reply = format!("Goal set: {}", goal.objective);
                    (Some(goal), reply)
                }
            }
        }
        GoalAction::Edit(objective) => {
            if objective.trim().is_empty() {
                return (
                    current,
                    "Goal error: objective cannot be empty. Usage: /goal edit <objective>"
                        .to_string(),
                );
            }
            match current {
                Some(mut goal) if !goal.status.is_terminal() => {
                    goal.edit(&objective);
                    let reply = format!("Goal updated: {}", goal.objective);
                    (Some(goal), reply)
                }
                Some(goal) => (
                    Some(goal),
                    "Goal error: goal is already complete. Clear it before editing.".to_string(),
                ),
                None => (
                    None,
                    "Goal error: goal not found. Start one with /goal start <objective>."
                        .to_string(),
                ),
            }
        }
        GoalAction::Pause(note) => match current {
            Some(mut goal) if goal.status.is_active() => {
                goal.set_status(GoalStatus::Paused, note);
                (Some(goal), "Goal paused.".to_string())
            }
            Some(ref goal) => {
                let status = goal.status;
                (
                    current,
                    format!("Goal error: cannot pause a {status} goal."),
                )
            }
            None => (
                None,
                "Goal error: goal not found. Start one with /goal start <objective>.".to_string(),
            ),
        },
        GoalAction::Resume(note) => match current {
            Some(mut goal) if goal.status.can_resume() => {
                goal.set_status(GoalStatus::Active, note);
                (Some(goal), "Goal resumed.".to_string())
            }
            Some(ref goal) if goal.status.is_active() => {
                (current, "Goal is already active.".to_string())
            }
            Some(ref goal) => {
                let status = goal.status;
                (
                    current,
                    format!("Goal error: cannot resume a {status} goal."),
                )
            }
            None => (
                None,
                "Goal error: goal not found. Start one with /goal start <objective>.".to_string(),
            ),
        },
        GoalAction::Complete(note) => match current {
            Some(mut goal) if !goal.status.is_terminal() => {
                goal.set_status(GoalStatus::Complete, note);
                (Some(goal), "Goal completed.".to_string())
            }
            Some(goal) => (
                Some(goal),
                "Goal error: goal is already complete.".to_string(),
            ),
            None => (
                None,
                "Goal error: goal not found. Start one with /goal start <objective>.".to_string(),
            ),
        },
        GoalAction::Block(note) => match current {
            Some(mut goal) if !goal.status.is_terminal() => {
                goal.set_status(GoalStatus::Blocked, note);
                (Some(goal), "Goal marked as blocked.".to_string())
            }
            Some(goal) => (
                Some(goal),
                "Goal error: goal is already complete.".to_string(),
            ),
            None => (
                None,
                "Goal error: goal not found. Start one with /goal start <objective>.".to_string(),
            ),
        },
        GoalAction::Clear => {
            if current.is_some() {
                (None, "Goal cleared.".to_string())
            } else {
                (None, "No active goal to clear.".to_string())
            }
        }
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "goal".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!("{file_name}.tmp-{}-{nanos}", std::process::id()))
}

fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.contains(['/', '\\'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_context_line_includes_objective() {
        let goal = Goal::new("get CI green");
        assert!(goal.context_line().contains("get CI green"));
        assert!(goal.context_line().contains("Active goal:"));
    }

    #[test]
    fn parse_empty_args_shows_status() {
        assert_eq!(parse_goal(""), GoalAction::Show);
        assert_eq!(parse_goal("   "), GoalAction::Show);
    }

    #[test]
    fn parse_start_verbs() {
        assert_eq!(
            parse_goal("start fix the bug"),
            GoalAction::Start("fix the bug".to_string())
        );
        assert_eq!(
            parse_goal("set fix the bug"),
            GoalAction::Start("fix the bug".to_string())
        );
        assert_eq!(
            parse_goal("create fix the bug"),
            GoalAction::Start("fix the bug".to_string())
        );
    }

    #[test]
    fn parse_unrecognized_verb_becomes_objective() {
        assert_eq!(
            parse_goal("fix the bug"),
            GoalAction::Start("fix the bug".to_string())
        );
    }

    #[test]
    fn parse_edit_pause_resume_complete_block_clear() {
        assert_eq!(
            parse_goal("edit fix the bug and docs"),
            GoalAction::Edit("fix the bug and docs".to_string())
        );
        assert_eq!(
            parse_goal("pause waiting for CI"),
            GoalAction::Pause(Some("waiting for CI".to_string()))
        );
        assert_eq!(parse_goal("pause"), GoalAction::Pause(None));
        assert_eq!(
            parse_goal("resume got green light"),
            GoalAction::Resume(Some("got green light".to_string()))
        );
        assert_eq!(
            parse_goal("complete pushed and verified"),
            GoalAction::Complete(Some("pushed and verified".to_string()))
        );
        assert_eq!(parse_goal("done"), GoalAction::Complete(None));
        assert_eq!(
            parse_goal("block upstream broken"),
            GoalAction::Block(Some("upstream broken".to_string()))
        );
        assert_eq!(parse_goal("blocked"), GoalAction::Block(None));
        assert_eq!(parse_goal("clear"), GoalAction::Clear);
    }

    #[test]
    fn apply_start_creates_goal() {
        let (goal, msg) = apply_action(None, GoalAction::Start("fix bug".to_string()));
        assert!(goal.is_some());
        assert_eq!(goal.as_ref().unwrap().objective, "fix bug");
        assert!(msg.contains("Goal set"));
    }

    #[test]
    fn apply_start_fails_when_goal_exists() {
        let existing = Goal::new("existing");
        let (goal, msg) = apply_action(Some(existing), GoalAction::Start("new".to_string()));
        assert!(goal.is_some());
        assert!(msg.contains("already exists"));
    }

    #[test]
    fn apply_start_replaces_terminal_goal() {
        let mut existing = Goal::new("old");
        existing.set_status(GoalStatus::Complete, None);
        let (goal, msg) = apply_action(Some(existing), GoalAction::Start("new".to_string()));
        assert_eq!(goal.as_ref().unwrap().objective, "new");
        assert!(msg.contains("Goal set"));
    }

    #[test]
    fn apply_complete_then_clear() {
        let goal = Goal::new("fix bug");
        let (goal, msg) = apply_action(Some(goal), GoalAction::Complete(None));
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Complete);
        assert!(msg.contains("completed"));

        let (goal, msg) = apply_action(goal, GoalAction::Clear);
        assert!(goal.is_none());
        assert!(msg.contains("cleared"));
    }

    #[test]
    fn apply_pause_resume_cycle() {
        let goal = Goal::new("fix bug");
        let (goal, _) = apply_action(Some(goal), GoalAction::Pause(None));
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Paused);

        let (goal, _) = apply_action(goal, GoalAction::Resume(None));
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Active);
    }

    #[test]
    fn apply_edit_keeps_status() {
        let mut goal = Goal::new("old");
        goal.set_status(GoalStatus::Paused, Some("waiting".to_string()));
        let (goal, _) = apply_action(Some(goal), GoalAction::Edit("new".to_string()));
        assert_eq!(goal.as_ref().unwrap().objective, "new");
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Paused);
    }

    #[test]
    fn apply_block_then_resume() {
        let goal = Goal::new("fix bug");
        let (goal, _) = apply_action(Some(goal), GoalAction::Block(Some("upstream".to_string())));
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Blocked);
        assert_eq!(goal.as_ref().unwrap().note, Some("upstream".to_string()));

        let (goal, _) = apply_action(goal, GoalAction::Resume(None));
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoalStore::new(dir.path());
        let key = "agent:main:dm:cli:default:direct:peer-1";

        assert!(store.load(key).await.unwrap().is_none());

        let goal = Goal::new("get CI green");
        store.save(key, &goal).await.unwrap();

        let loaded = store.load(key).await.unwrap().unwrap();
        assert_eq!(loaded.objective, goal.objective);
        assert_eq!(loaded.status, goal.status);
    }

    #[tokio::test]
    async fn store_remove_clears_goal() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoalStore::new(dir.path());
        let key = "agent:main:dm:cli:default:direct:peer-2";

        let goal = Goal::new("x");
        store.save(key, &goal).await.unwrap();
        store.remove(key).await.unwrap();

        assert!(store.load(key).await.unwrap().is_none());
    }

    #[test]
    fn store_rejects_malformed_session_key() {
        let store = GoalStore::default();
        assert!(store.path_for("not-a-session-key").is_err());
        assert!(
            store
                .path_for("agent:main:dm:cli:default:direct:../evil")
                .is_err()
        );
    }
}
