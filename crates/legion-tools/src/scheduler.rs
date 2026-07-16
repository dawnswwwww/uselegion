//! Agent-callable scheduler tools for managing cron jobs.
//!
//! These tools read and write `~/.legion/automation/cron.jsonl` via the same
//! JSONL store used by the background cron scheduler, so agent-created jobs
//! are picked up by the scheduler loop automatically.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use legion_automation::cron::{CronJob, CronJobStore, JsonlCronJobStore};
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde_json::json;

use crate::policy::{Approval, Policy};

macro_rules! legion_tool_taxonomy {
    ($kind:expr) => {
        fn kind(&self) -> ToolKind {
            $kind
        }
        fn namespace(&self) -> ToolNamespace {
            ToolNamespace::Legion
        }
    };
}

/// Default path to the shared cron job store.
fn default_cron_store_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".legion").join("automation").join("cron.jsonl"))
        .unwrap_or_else(|| PathBuf::from(".legion/automation/cron.jsonl"))
}

/// Open the cron store at the configured or default path.
async fn open_cron_store(path: Option<&PathBuf>) -> Result<Arc<dyn CronJobStore>, ToolError> {
    let path = path.cloned().unwrap_or_else(default_cron_store_path);
    let store = JsonlCronJobStore::open(path)
        .await
        .map_err(|e| ToolError::Execution(format!("failed to open cron store: {e}")))?;
    Ok(Arc::new(store))
}

/// Generate a unique job id in the same shape as the cron scheduler.
fn generate_job_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!(
        "cron-{}-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Accept 5-field cron expressions (minute hour dom month dow) and 6-field
/// expressions (with seconds), normalizing the former to run at second 0.
fn normalize_cron_expression(expression: &str) -> Result<String, ToolError> {
    let trimmed = expression.trim();
    let field_count = trimmed.split_whitespace().count();
    match field_count {
        5 => Ok(format!("0 {}", trimmed)),
        6 => Ok(trimmed.to_string()),
        _ => Err(ToolError::InvalidParams(format!(
            "cron expression must have 5 or 6 fields, got {field_count}"
        ))),
    }
}

/// Validate that `expression` is a parseable cron schedule.
fn validate_cron(expression: &str) -> Result<(), ToolError> {
    let normalized = normalize_cron_expression(expression)?;
    cron::Schedule::from_str(&normalized)
        .map(|_| ())
        .map_err(|e| ToolError::InvalidParams(format!("invalid cron expression: {e}")))
}

fn scheduler_policy(approval: Approval) -> Policy {
    Policy {
        approval,
        permission_mode: None,
        allow_from: vec![],
        workspace_only: false,
    }
}

// ---------------------------------------------------------------------------
// scheduler_create
// ---------------------------------------------------------------------------

/// Create a new scheduled cron job.
pub struct SchedulerCreateTool {
    store_path: Option<PathBuf>,
}

impl SchedulerCreateTool {
    pub fn new() -> Self {
        Self { store_path: None }
    }

    /// Construct a tool that reads/writes a specific store path (used in tests).
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            store_path: Some(path.into()),
        }
    }
}

impl Default for SchedulerCreateTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SchedulerCreateTool {
    fn name(&self) -> &str {
        "scheduler_create"
    }

    fn description(&self) -> &str {
        "Create a new recurring cron job with a name, schedule, and prompt."
    }

    fn policy(&self) -> &Policy {
        static POLICY: std::sync::OnceLock<Policy> = std::sync::OnceLock::new();
        POLICY.get_or_init(|| scheduler_policy(Approval::Prompt))
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "human-readable job name" },
                "cron": { "type": "string", "description": "cron expression (5 or 6 fields)" },
                "prompt": { "type": "string", "description": "prompt passed to the agent on each run" },
                "agent_type": { "type": "string", "description": "agent type to run (defaults to the calling agent)" },
                "enabled": { "type": "boolean", "description": "whether the job is enabled (defaults to true)" }
            },
            "required": ["name", "cron", "prompt"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    legion_tool_taxonomy!(ToolKind::Other);

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let name = params["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'name' parameter".to_string()))?;
        let cron_expr = params["cron"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'cron' parameter".to_string()))?;
        let prompt = params["prompt"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;
        let agent_type = params["agent_type"]
            .as_str()
            .unwrap_or(&ctx.agent_id)
            .to_string();
        let enabled = params["enabled"].as_bool().unwrap_or(true);

        validate_cron(cron_expr)?;

        let store = open_cron_store(self.store_path.as_ref()).await?;
        let mut job = CronJob {
            id: generate_job_id(),
            agent_id: agent_type,
            message: prompt.to_string(),
            name: name.to_string(),
            schedule: cron_expr.to_string(),
            at: None,
            enabled,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            webhook_secret: None,
        };
        job.refresh_next_run();
        let id = job.id.clone();
        store
            .create(job)
            .await
            .map_err(|e| ToolError::Execution(format!("failed to create cron job: {e}")))?;

        Ok(ToolResult::ok(json!({ "id": id }).to_string()))
    }
}

// ---------------------------------------------------------------------------
// scheduler_delete
// ---------------------------------------------------------------------------

/// Remove a scheduled cron job by id.
pub struct SchedulerDeleteTool {
    store_path: Option<PathBuf>,
}

impl SchedulerDeleteTool {
    pub fn new() -> Self {
        Self { store_path: None }
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            store_path: Some(path.into()),
        }
    }
}

impl Default for SchedulerDeleteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SchedulerDeleteTool {
    fn name(&self) -> &str {
        "scheduler_delete"
    }

    fn description(&self) -> &str {
        "Delete a scheduled cron job by its id."
    }

    fn policy(&self) -> &Policy {
        static POLICY: std::sync::OnceLock<Policy> = std::sync::OnceLock::new();
        POLICY.get_or_init(|| scheduler_policy(Approval::Prompt))
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "id of the job to delete" }
            },
            "required": ["id"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_destructive(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    legion_tool_taxonomy!(ToolKind::Delete);

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let id = params["id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'id' parameter".to_string()))?;

        let store = open_cron_store(self.store_path.as_ref()).await?;
        store
            .remove(id)
            .await
            .map_err(|e| ToolError::Execution(format!("failed to delete cron job '{id}': {e}")))?;

        Ok(ToolResult::ok(format!("deleted job '{id}'")))
    }
}

// ---------------------------------------------------------------------------
// scheduler_list
// ---------------------------------------------------------------------------

/// List all scheduled cron jobs.
pub struct SchedulerListTool {
    store_path: Option<PathBuf>,
}

impl SchedulerListTool {
    pub fn new() -> Self {
        Self { store_path: None }
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            store_path: Some(path.into()),
        }
    }
}

impl Default for SchedulerListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SchedulerListTool {
    fn name(&self) -> &str {
        "scheduler_list"
    }

    fn description(&self) -> &str {
        "List all scheduled cron jobs with their id, name, schedule, prompt, and status."
    }

    fn policy(&self) -> &Policy {
        static POLICY: std::sync::OnceLock<Policy> = std::sync::OnceLock::new();
        POLICY.get_or_init(|| scheduler_policy(Approval::Off))
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    legion_tool_taxonomy!(ToolKind::ListDir);

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let store = open_cron_store(self.store_path.as_ref()).await?;
        let jobs = store
            .list()
            .await
            .map_err(|e| ToolError::Execution(format!("failed to list cron jobs: {e}")))?;

        let out: Vec<serde_json::Value> = jobs
            .into_iter()
            .map(|job| {
                json!({
                    "id": job.id,
                    "name": job.name,
                    "cron": job.schedule,
                    "prompt": job.message,
                    "agent_type": job.agent_id,
                    "enabled": job.enabled,
                })
            })
            .collect();

        Ok(ToolResult::ok(json!(out).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::ToolContext;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn test_ctx() -> ToolContext {
        ToolContext {
            workspace: PathBuf::from("/tmp"),
            session_id: "test-session".to_string(),
            agent_id: "test-agent".to_string(),
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

    #[tokio::test]
    async fn create_job_and_read_it_back() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cron.jsonl");

        let tool = SchedulerCreateTool::with_path(&path);
        let res = tool
            .execute(
                json!({
                    "name": "daily summary",
                    "cron": "0 9 * * *",
                    "prompt": "summarize yesterday",
                    "agent_type": "main"
                }),
                test_ctx(),
            )
            .await
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&res.content).unwrap();
        let id = parsed["id"].as_str().unwrap();
        assert!(id.starts_with("cron-"));

        let store = JsonlCronJobStore::open(&path).await.unwrap();
        let jobs = store.list().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].name, "daily summary");
        assert_eq!(jobs[0].schedule, "0 9 * * *");
        assert_eq!(jobs[0].message, "summarize yesterday");
        assert_eq!(jobs[0].agent_id, "main");
        assert!(jobs[0].enabled);
        assert!(jobs[0].next_run.is_some());
    }

    #[tokio::test]
    async fn create_rejects_invalid_cron() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cron.jsonl");

        let tool = SchedulerCreateTool::with_path(&path);
        let err = tool
            .execute(
                json!({
                    "name": "bad",
                    "cron": "not a cron",
                    "prompt": "x"
                }),
                test_ctx(),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("cron expression"));
    }

    #[tokio::test]
    async fn delete_job_removes_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cron.jsonl");

        let create = SchedulerCreateTool::with_path(&path);
        let res = create
            .execute(
                json!({
                    "name": "to delete",
                    "cron": "0 9 * * *",
                    "prompt": "x"
                }),
                test_ctx(),
            )
            .await
            .unwrap();
        let id = serde_json::from_str::<serde_json::Value>(&res.content).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let delete = SchedulerDeleteTool::with_path(&path);
        let res = delete
            .execute(json!({ "id": id }), test_ctx())
            .await
            .unwrap();
        assert!(res.content.contains("deleted"));

        let store = JsonlCronJobStore::open(&path).await.unwrap();
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_jobs_returns_created_jobs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cron.jsonl");

        let create = SchedulerCreateTool::with_path(&path);
        create
            .execute(
                json!({
                    "name": "first",
                    "cron": "0 9 * * *",
                    "prompt": "one"
                }),
                test_ctx(),
            )
            .await
            .unwrap();
        create
            .execute(
                json!({
                    "name": "second",
                    "cron": "0 10 * * *",
                    "prompt": "two",
                    "enabled": false
                }),
                test_ctx(),
            )
            .await
            .unwrap();

        let list = SchedulerListTool::with_path(&path);
        let res = list.execute(json!({}), test_ctx()).await.unwrap();
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&res.content).unwrap();
        assert_eq!(jobs.len(), 2);

        let names: HashSet<&str> = jobs.iter().filter_map(|j| j["name"].as_str()).collect();
        assert!(names.contains("first"));
        assert!(names.contains("second"));

        let second = jobs.iter().find(|j| j["name"] == "second").unwrap();
        assert_eq!(second["cron"], "0 10 * * *");
        assert_eq!(second["enabled"], false);
    }
}
