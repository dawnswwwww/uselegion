//! Session goal tools: model-facing management of the session goal.
//!
//! - `get_goal`: show the current session goal.
//! - `create_goal`: start a new session goal.
//! - `update_goal`: change status, objective, note, or turn budget.
//!
//! Goals are persisted by [`GoalStore`] at
//! `~/.legion/agents/<agent>/goals/<peer>.json`, derived from the session
//! key. While a goal is active, the turn-end goal gate
//! (`legion_runtime::goal_gate`) keeps the run going ("goal turns"), so
//! `update_goal` is how the model ends the pursuit: `complete`, `blocked`,
//! or `paused`.
//!
//! Permission boundary (same as `session_tools`): the goal file is derived
//! from `ctx.session_id` only, and keys whose agent segment does not match
//! the calling agent are rejected.

use async_trait::async_trait;
use legion_plugin_sdk::session_key::parse_session_key;
use legion_runtime::goal::{Goal, GoalAction, GoalStore, apply_action};
use legion_runtime::tools::{Policy, Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

/// Validate the session key in `ctx` and its agent segment.
fn check_session(ctx: &ToolContext) -> Result<(), String> {
    let parts = parse_session_key(&ctx.session_id)
        .ok_or_else(|| format!("invalid session key: {}", ctx.session_id))?;
    if parts.agent_id != ctx.agent_id {
        return Err("cross-agent session access denied".to_string());
    }
    Ok(())
}

async fn load_goal(store: &GoalStore, ctx: &ToolContext) -> Result<Option<Goal>, ToolError> {
    store
        .load(&ctx.session_id)
        .await
        .map_err(|e| ToolError::Execution(format!("goal store: {e}")))
}

async fn save_goal(store: &GoalStore, ctx: &ToolContext, goal: &Goal) -> Result<(), ToolError> {
    store
        .save(&ctx.session_id, goal)
        .await
        .map_err(|e| ToolError::Execution(format!("goal store: {e}")))
}

/// `get_goal`: show the current session goal.
pub struct GetGoalTool {
    store: GoalStore,
    policy: Policy,
}

impl GetGoalTool {
    pub fn new(policy: Policy) -> Self {
        Self {
            store: GoalStore::default(),
            policy,
        }
    }

    pub fn with_store(store: GoalStore, policy: Policy) -> Self {
        Self { store, policy }
    }
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "get_goal"
    }

    fn description(&self) -> &str {
        "Show the current session goal: status, objective, note, and goal-turn budget."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, _params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        if let Err(msg) = check_session(&ctx) {
            return Ok(ToolResult::error(msg));
        }
        let (current, reply) = apply_action(load_goal(&self.store, &ctx).await?, GoalAction::Show);
        let _ = current;
        Ok(ToolResult::ok(reply))
    }
}

/// `create_goal`: start a new session goal.
pub struct CreateGoalTool {
    store: GoalStore,
    policy: Policy,
}

impl CreateGoalTool {
    pub fn new(policy: Policy) -> Self {
        Self {
            store: GoalStore::default(),
            policy,
        }
    }

    pub fn with_store(store: GoalStore, policy: Policy) -> Self {
        Self { store, policy }
    }
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "create_goal"
    }

    fn description(&self) -> &str {
        "Start a new session goal with the given objective. Only one goal can \
         exist per session; creating fails while a non-terminal goal exists \
         (use update_goal to reword it). While a goal is active the run keeps \
         going in goal turns until it is completed, blocked, paused, or out \
         of goal-turn budget."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "What the session should achieve."
                }
            },
            "required": ["objective"],
            "additionalProperties": false
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        if let Err(msg) = check_session(&ctx) {
            return Ok(ToolResult::error(msg));
        }
        let objective = params
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let current = load_goal(&self.store, &ctx).await?;
        let had_goal = current.is_some();
        let (new_goal, reply) = apply_action(current, GoalAction::Start(objective));
        let Some(goal) = new_goal else {
            return Ok(ToolResult::error(reply));
        };
        // Start only replaces a terminal goal; treat an unchanged pre-existing
        // goal as the "already exists" failure apply_action reported.
        if had_goal && !reply.starts_with("Goal set") {
            return Ok(ToolResult::error(reply));
        }
        save_goal(&self.store, &ctx, &goal).await?;
        Ok(ToolResult::ok(reply))
    }
}

/// `update_goal`: change the current session goal.
pub struct UpdateGoalTool {
    store: GoalStore,
    policy: Policy,
}

impl UpdateGoalTool {
    pub fn new(policy: Policy) -> Self {
        Self {
            store: GoalStore::default(),
            policy,
        }
    }

    pub fn with_store(store: GoalStore, policy: Policy) -> Self {
        Self { store, policy }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "update_goal"
    }

    fn description(&self) -> &str {
        "Update the current session goal. Set `status` to \"complete\" when the \
         goal is achieved, \"blocked\" when genuinely stuck, \"paused\" when \
         user input is required, or \"active\" to resume. Optionally reword \
         `objective` or attach a `note` to a status change."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["active", "paused", "blocked", "complete"],
                    "description": "New lifecycle status."
                },
                "note": {
                    "type": "string",
                    "description": "Optional note recorded with the status change."
                },
                "objective": {
                    "type": "string",
                    "description": "Reword the objective."
                }
            },
            "additionalProperties": false
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        if let Err(msg) = check_session(&ctx) {
            return Ok(ToolResult::error(msg));
        }
        let note = params
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_string);

        // Build the action list in a fixed order: reword, then status.
        let mut actions = Vec::new();
        if let Some(objective) = params.get("objective").and_then(Value::as_str) {
            actions.push(GoalAction::Edit(objective.to_string()));
        }
        if let Some(status) = params.get("status").and_then(Value::as_str) {
            let action = match status {
                "active" => GoalAction::Resume(note.clone()),
                "paused" => GoalAction::Pause(note.clone()),
                "blocked" => GoalAction::Block(note.clone()),
                "complete" => GoalAction::Complete(note.clone()),
                other => {
                    return Ok(ToolResult::error(format!(
                        "invalid status {other:?} (expected active|paused|blocked|complete)"
                    )));
                }
            };
            actions.push(action);
        } else if note.is_some() {
            return Ok(ToolResult::error(
                "note requires a status change".to_string(),
            ));
        }
        if actions.is_empty() {
            return Ok(ToolResult::error(
                "no update fields provided (status or objective)".to_string(),
            ));
        }

        let mut current = load_goal(&self.store, &ctx).await?;
        let mut replies = Vec::new();
        for action in actions {
            let (next, reply) = apply_action(current, action);
            current = next;
            let failed = reply.starts_with("Goal error:");
            replies.push(reply);
            if failed {
                // All-or-nothing: a failed action aborts the update without
                // persisting, so the model can correct itself and retry.
                return Ok(ToolResult::error(replies.join("\n")));
            }
        }
        match &current {
            Some(goal) => save_goal(&self.store, &ctx, goal).await?,
            None => self
                .store
                .remove(&ctx.session_id)
                .await
                .map_err(|e| ToolError::Execution(format!("goal store: {e}")))?,
        }
        Ok(ToolResult::ok(replies.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::goal::GoalStatus;
    use legion_runtime::tools::Approval;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn policy() -> Policy {
        Policy {
            approval: Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    fn store() -> (GoalStore, TempDir) {
        let dir = TempDir::new().unwrap();
        (GoalStore::new(dir.path()), dir)
    }

    fn session_key(agent_id: &str, peer_id: &str) -> String {
        legion_plugin_sdk::session_key::direct_session_key(
            agent_id, "dm", "webchat", "default", peer_id,
        )
    }

    fn ctx(agent_id: &str, session_key: &str) -> ToolContext {
        ToolContext {
            workspace: PathBuf::from("/tmp"),
            session_id: session_key.to_string(),
            agent_id: agent_id.to_string(),
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
            plan_mode_tracker: None,
        }
    }

    const PEER: &str = "user1";

    // ---- get_goal ----

    #[tokio::test]
    async fn get_goal_reports_none_when_empty() {
        let (store, _dir) = store();
        let tool = GetGoalTool::with_store(store, policy());
        let result = tool
            .execute(json!({}), ctx("main", &session_key("main", PEER)))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("No active goal"));
    }

    #[tokio::test]
    async fn get_goal_denies_cross_agent_key() {
        let (store, _dir) = store();
        let tool = GetGoalTool::with_store(store, policy());
        let result = tool
            .execute(json!({}), ctx("other", &session_key("main", PEER)))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("cross-agent"));
    }

    #[tokio::test]
    async fn get_goal_invalid_key_errors() {
        let (store, _dir) = store();
        let tool = GetGoalTool::with_store(store, policy());
        let result = tool
            .execute(json!({}), ctx("main", "not-a-session-key"))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("invalid session key"));
    }

    // ---- create_goal ----

    #[tokio::test]
    async fn create_then_get_goal() {
        let (store, _dir) = store();
        let key = session_key("main", PEER);

        let create = CreateGoalTool::with_store(store.clone(), policy());
        let result = create
            .execute(json!({"objective": "ship it"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Goal set: ship it"));

        let get = GetGoalTool::with_store(store, policy());
        let result = get.execute(json!({}), ctx("main", &key)).await.unwrap();
        assert!(result.content.contains("Status: active"));
        assert!(result.content.contains("Objective: ship it"));
    }

    #[tokio::test]
    async fn create_fails_when_goal_exists() {
        let (store, _dir) = store();
        let key = session_key("main", PEER);
        store.save(&key, &Goal::new("existing")).await.unwrap();

        let create = CreateGoalTool::with_store(store, policy());
        let result = create
            .execute(json!({"objective": "new"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("already exists"));
    }

    #[tokio::test]
    async fn create_replaces_terminal_goal() {
        let (store, _dir) = store();
        let key = session_key("main", PEER);
        let mut old = Goal::new("old");
        old.set_status(GoalStatus::Complete, None);
        store.save(&key, &old).await.unwrap();

        let create = CreateGoalTool::with_store(store, policy());
        let result = create
            .execute(json!({"objective": "new"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Goal set: new"));
    }

    #[tokio::test]
    async fn create_rejects_empty_objective() {
        let (store, _dir) = store();
        let create = CreateGoalTool::with_store(store, policy());
        let result = create
            .execute(
                json!({"objective": "  "}),
                ctx("main", &session_key("main", PEER)),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("objective cannot be empty"));
    }

    // ---- update_goal ----

    #[tokio::test]
    async fn update_status_transitions() {
        let (store, _dir) = store();
        let key = session_key("main", PEER);
        store.save(&key, &Goal::new("fix bug")).await.unwrap();

        let update = UpdateGoalTool::with_store(store.clone(), policy());
        let result = update
            .execute(
                json!({"status": "blocked", "note": "upstream"}),
                ctx("main", &key),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let goal = store.load(&key).await.unwrap().unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
        assert_eq!(goal.note.as_deref(), Some("upstream"));

        let result = update
            .execute(json!({"status": "active"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let goal = store.load(&key).await.unwrap().unwrap();
        assert_eq!(goal.status, GoalStatus::Active);

        let result = update
            .execute(json!({"status": "complete"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let goal = store.load(&key).await.unwrap().unwrap();
        assert_eq!(goal.status, GoalStatus::Complete);
    }

    #[tokio::test]
    async fn update_rewords_objective() {
        let (store, _dir) = store();
        let key = session_key("main", PEER);
        store.save(&key, &Goal::new("old")).await.unwrap();

        let update = UpdateGoalTool::with_store(store.clone(), policy());
        let result = update
            .execute(json!({"objective": "new wording"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let goal = store.load(&key).await.unwrap().unwrap();
        assert_eq!(goal.objective, "new wording");
    }

    #[tokio::test]
    async fn update_requires_existing_goal() {
        let (store, _dir) = store();
        let update = UpdateGoalTool::with_store(store, policy());
        let result = update
            .execute(
                json!({"status": "complete"}),
                ctx("main", &session_key("main", PEER)),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn update_rejects_bad_params() {
        let (store, _dir) = store();
        let key = session_key("main", PEER);
        store.save(&key, &Goal::new("fix bug")).await.unwrap();

        let update = UpdateGoalTool::with_store(store, policy());

        let result = update.execute(json!({}), ctx("main", &key)).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("no update fields"));

        let result = update
            .execute(json!({"status": "bogus"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("invalid status"));

        let result = update
            .execute(json!({"note": "dangling"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("note requires a status"));
    }
}
