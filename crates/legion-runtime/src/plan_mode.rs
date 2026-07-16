//! Plan-mode state machine for Grok CLI-style planning sessions.
//!
//! Plan mode restricts the agent to read-only tools and writes to a single
//! plan file. It is toggled by the `enter_plan_mode` and `exit_plan_mode`
//! tools implemented in `legion-tools`.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Lifecycle state of the plan-mode tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanModeState {
    /// Plan mode is not engaged.
    Inactive,
    /// Plan mode was requested externally but the model has not yet processed
    /// the request. Reserved for future UI toggles.
    Pending,
    /// Plan mode is active: mutating tools are restricted to the plan file.
    Active,
    /// Plan mode was requested to exit; the current turn is allowed to finish
    /// before the mode becomes fully inactive.
    ExitPending,
}

/// Persistable snapshot of plan-mode state.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanModeSnapshot {
    state: PlanModeState,
}

/// Tracks whether plan mode is active for a session and where the plan file
/// lives.
pub struct PlanModeTracker {
    state: PlanModeState,
    session_dir: PathBuf,
    plan_file_path: PathBuf,
}

const PLAN_FILE_NAME: &str = "plan.md";
const SNAPSHOT_FILE_NAME: &str = "plan_mode.json";

impl PlanModeTracker {
    /// Create a new tracker for `session_dir`. The plan file is
    /// `<session_dir>/plan.md`.
    pub fn new(session_dir: impl AsRef<Path>) -> Self {
        let session_dir = session_dir.as_ref().to_path_buf();
        let plan_file_path = session_dir.join(PLAN_FILE_NAME);
        Self {
            state: PlanModeState::Inactive,
            session_dir,
            plan_file_path,
        }
    }

    /// Engage plan mode. Idempotent from any state.
    pub fn activate(&mut self) {
        self.state = PlanModeState::Active;
    }

    /// Request to leave plan mode. Moves directly to inactive from any state.
    ///
    /// [`PlanModeState::ExitPending`] is reserved for external UI toggles and
    /// is handled by [`Self::finalize_exit_if_pending`].
    pub fn deactivate(&mut self) {
        self.state = PlanModeState::Inactive;
    }

    /// If the tracker is waiting to exit, complete the transition to inactive.
    /// Call this at turn end.
    pub fn finalize_exit_if_pending(&mut self) {
        if self.state == PlanModeState::ExitPending {
            self.state = PlanModeState::Inactive;
        }
    }

    /// Returns `true` when plan-mode restrictions should be enforced (active or
    /// finishing the current turn after an exit request).
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            PlanModeState::Active | PlanModeState::ExitPending
        )
    }

    /// Current state.
    pub fn state(&self) -> PlanModeState {
        self.state
    }

    /// Absolute path to the plan file.
    pub fn plan_file_path(&self) -> &Path {
        &self.plan_file_path
    }

    /// Returns `true` when `path` is the plan file and plan mode is active.
    ///
    /// `path` should already be resolved to an absolute path for reliable
    /// comparison.
    pub fn should_auto_approve_edit(&self, path: impl AsRef<Path>) -> bool {
        if !self.is_active() {
            return false;
        }
        paths_equal(path.as_ref(), &self.plan_file_path)
    }

    /// Persist state to `<session_dir>/plan_mode.json`.
    pub async fn save(&self) -> std::io::Result<()> {
        let path = self.session_dir.join(SNAPSHOT_FILE_NAME);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let snapshot = PlanModeSnapshot { state: self.state };
        let content = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&path, content).await
    }

    /// Load a tracker from `<session_dir>/plan_mode.json`, falling back to a
    /// fresh inactive tracker if the file is missing.
    pub async fn load(session_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let path = session_dir.join(SNAPSHOT_FILE_NAME);
        if !path.exists() {
            return Ok(Self::new(&session_dir));
        }
        let content = tokio::fs::read_to_string(&path).await?;
        let snapshot: PlanModeSnapshot = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut tracker = Self::new(&session_dir);
        tracker.state = snapshot.state;
        Ok(tracker)
    }
}

/// Compare two paths, normalizing `.`/`..` components. Falls back to component
/// comparison if canonicalization fails (e.g. the file does not exist yet).
fn paths_equal(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    normalize_path(a) == normalize_path(b)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_is_inactive() {
        let tracker = PlanModeTracker::new("/tmp/sessions/s1");
        assert_eq!(tracker.state(), PlanModeState::Inactive);
        assert!(!tracker.is_active());
    }

    #[test]
    fn activate_and_deactivate() {
        let mut tracker = PlanModeTracker::new("/tmp/sessions/s1");
        tracker.activate();
        assert_eq!(tracker.state(), PlanModeState::Active);
        assert!(tracker.is_active());

        tracker.deactivate();
        assert_eq!(tracker.state(), PlanModeState::Inactive);
        assert!(!tracker.is_active());
    }

    #[test]
    fn activate_cancels_exit_pending() {
        let dir = tempfile::tempdir().unwrap();
        // Serialize an ExitPending state and load it to exercise the variant.
        let snapshot = r#"{"state":"exit_pending"}"#;
        std::fs::write(dir.path().join("plan_mode.json"), snapshot).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut tracker = runtime.block_on(PlanModeTracker::load(dir.path())).unwrap();
        assert_eq!(tracker.state(), PlanModeState::ExitPending);

        tracker.activate();
        assert_eq!(tracker.state(), PlanModeState::Active);
        assert!(tracker.is_active());
    }

    #[test]
    fn deactivate_from_inactive_is_noop() {
        let mut tracker = PlanModeTracker::new("/tmp/sessions/s1");
        tracker.deactivate();
        assert_eq!(tracker.state(), PlanModeState::Inactive);
        assert!(!tracker.is_active());
    }

    #[test]
    fn plan_file_path_returns_session_dir_plan_md() {
        let tracker = PlanModeTracker::new("/tmp/sessions/s1");
        assert_eq!(
            tracker.plan_file_path(),
            Path::new("/tmp/sessions/s1/plan.md")
        );
    }

    #[test]
    fn should_auto_approve_edit_requires_active_mode() {
        let tracker = PlanModeTracker::new("/tmp/sessions/s1");
        assert!(!tracker.should_auto_approve_edit("/tmp/sessions/s1/plan.md"));

        let mut active = PlanModeTracker::new("/tmp/sessions/s1");
        active.activate();
        assert!(active.should_auto_approve_edit("/tmp/sessions/s1/plan.md"));
        assert!(!active.should_auto_approve_edit("/tmp/sessions/s1/other.md"));
    }

    #[test]
    fn should_auto_approve_edit_normalizes_path() {
        let mut tracker = PlanModeTracker::new("/tmp/sessions/s1");
        tracker.activate();
        assert!(tracker.should_auto_approve_edit("/tmp/sessions/s1/./plan.md"));
        assert!(tracker.should_auto_approve_edit("/tmp/sessions/../sessions/s1/plan.md"));
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = PlanModeTracker::new(dir.path());
        tracker.activate();
        tracker.save().await.unwrap();

        let loaded = PlanModeTracker::load(dir.path()).await.unwrap();
        assert_eq!(loaded.state(), PlanModeState::Active);
        assert_eq!(loaded.plan_file_path(), tracker.plan_file_path());
    }

    #[tokio::test]
    async fn load_missing_returns_inactive() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = PlanModeTracker::load(dir.path()).await.unwrap();
        assert_eq!(tracker.state(), PlanModeState::Inactive);
    }
}
