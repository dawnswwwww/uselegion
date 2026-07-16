//! Plan-mode toggle tools.
//!
//! These tools let the model enter and exit Grok CLI-style plan mode. While
//! plan mode is active, the runtime restricts mutating tools to the session
//! plan file.

use async_trait::async_trait;
use legion_runtime::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::json;

use crate::policy::{Approval, Policy};

/// Activate plan mode for the current session.
pub struct EnterPlanModeTool {
    policy: Policy,
}

impl EnterPlanModeTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

impl Default for EnterPlanModeTool {
    fn default() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Off,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "enter_plan_mode"
    }

    fn description(&self) -> &str {
        "Enter plan mode. In plan mode, only read-only tools and writes to the session plan file are allowed."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        // Mutates the session plan-mode state.
        false
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        match &ctx.plan_mode_tracker {
            Some(tracker) => {
                let mut guard = tracker.lock().await;
                let was_active = guard.is_active();
                guard.activate();
                if was_active {
                    Ok(ToolResult::ok("Plan mode is already active."))
                } else {
                    Ok(ToolResult::ok(format!(
                        "Plan mode activated. You may write only to the plan file ({}). Read-only tools remain available. Use exit_plan_mode when you are ready to execute.",
                        guard.plan_file_path().display()
                    )))
                }
            }
            None => Ok(ToolResult::error(
                "plan mode tracker is not available in this context",
            )),
        }
    }
}

/// Request to leave plan mode. Subject to approval policy.
pub struct ExitPlanModeTool {
    policy: Policy,
}

impl ExitPlanModeTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

impl Default for ExitPlanModeTool {
    fn default() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Prompt,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "exit_plan_mode"
    }

    fn description(&self) -> &str {
        "Request to exit plan mode. Requires approval based on the configured policy."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        match &ctx.plan_mode_tracker {
            Some(tracker) => {
                let mut guard = tracker.lock().await;
                if !guard.is_active() {
                    return Ok(ToolResult::ok("Plan mode is not active."));
                }
                guard.deactivate();
                Ok(ToolResult::ok("Plan mode deactivated."))
            }
            None => Ok(ToolResult::error(
                "plan mode tracker is not available in this context",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::PlanModeTracker;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx(tracker: Option<Arc<tokio::sync::Mutex<PlanModeTracker>>>) -> ToolContext {
        ToolContext {
            workspace: std::path::PathBuf::from("/tmp"),
            session_id: "s1".into(),
            agent_id: "a1".into(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: None,
            swarm: None,
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
            background_tasks: None,
            plan_mode_tracker: tracker,
        }
    }

    #[tokio::test]
    async fn enter_plan_mode_activates_tracker() {
        let dir = TempDir::new().unwrap();
        let tracker = Arc::new(tokio::sync::Mutex::new(PlanModeTracker::new(dir.path())));
        let tool = EnterPlanModeTool::default();

        let result = tool
            .execute(json!({}), ctx(Some(tracker.clone())))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(tracker.lock().await.is_active());
        assert!(result.content.contains("Plan mode activated"));
    }

    #[tokio::test]
    async fn enter_plan_mode_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let tracker = Arc::new(tokio::sync::Mutex::new(PlanModeTracker::new(dir.path())));
        let tool = EnterPlanModeTool::default();

        tool.execute(json!({}), ctx(Some(tracker.clone())))
            .await
            .unwrap();
        let result = tool
            .execute(json!({}), ctx(Some(tracker.clone())))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("already active"));
        assert!(tracker.lock().await.is_active());
    }

    #[tokio::test]
    async fn enter_plan_mode_without_tracker_errors() {
        let tool = EnterPlanModeTool::default();
        let result = tool.execute(json!({}), ctx(None)).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn exit_plan_mode_deactivates_tracker() {
        let dir = TempDir::new().unwrap();
        let tracker = Arc::new(tokio::sync::Mutex::new(PlanModeTracker::new(dir.path())));
        tracker.lock().await.activate();

        let tool = ExitPlanModeTool::default();
        let result = tool
            .execute(json!({}), ctx(Some(tracker.clone())))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(!tracker.lock().await.is_active());
    }

    #[tokio::test]
    async fn exit_plan_mode_when_inactive_reports_inactive() {
        let dir = TempDir::new().unwrap();
        let tracker = Arc::new(tokio::sync::Mutex::new(PlanModeTracker::new(dir.path())));

        let tool = ExitPlanModeTool::default();
        let result = tool
            .execute(json!({}), ctx(Some(tracker.clone())))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("not active"));
    }
}
