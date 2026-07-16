use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use legion_runtime::{
    BackgroundTaskOutput, BackgroundTaskRegistry as BackgroundTaskRegistryTrait,
    BackgroundTaskResult, Tool, ToolContext, ToolError, ToolResult,
};
use serde_json::json;
use tokio::sync::Mutex;

use crate::policy::{Approval, Policy};

/// Default base directory for session-local task logs.
pub fn default_task_log_dir(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .map(|h| {
            h.join(".legion")
                .join("sessions")
                .join(session_id)
                .join("tasks")
        })
        .unwrap_or_else(|| {
            PathBuf::from(".legion")
                .join("sessions")
                .join(session_id)
                .join("tasks")
        })
}

/// Build the log file path for a given session and task id.
pub fn task_log_path(session_id: &str, task_id: &str) -> PathBuf {
    default_task_log_dir(session_id).join(format!("{task_id}.log"))
}

/// Internal state of a tracked background task.
enum TaskState {
    Running(tokio::task::JoinHandle<Result<BackgroundTaskResult, String>>),
    Completed(Result<BackgroundTaskResult, String>),
}

impl std::fmt::Debug for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskState::Running(_) => f.debug_tuple("Running").field(&"<handle>").finish(),
            TaskState::Completed(result) => f.debug_tuple("Completed").field(result).finish(),
        }
    }
}

#[derive(Debug, Clone)]
struct TaskEntry {
    state: Arc<Mutex<TaskState>>,
    log_path: PathBuf,
}

/// Thread-safe registry mapping task ids to their running handles, log paths,
/// and completion status.
#[derive(Debug)]
pub struct BackgroundTaskRegistry {
    tasks: Mutex<HashMap<String, Arc<TaskEntry>>>,
    counter: AtomicU64,
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
        }
    }

    /// Ensure the parent directories for a task log exist.
    pub async fn ensure_log_dir(log_path: &Path) -> Result<(), ToolError> {
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::Execution(format!(
                    "failed to create task log directory '{}': {e}",
                    parent.display()
                ))
            })?;
        }
        Ok(())
    }
}

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BackgroundTaskRegistryTrait for BackgroundTaskRegistry {
    fn next_task_id(&self) -> String {
        format!("task-{}", self.counter.fetch_add(1, Ordering::SeqCst))
    }

    async fn register(
        &self,
        task_id: String,
        handle: tokio::task::JoinHandle<Result<BackgroundTaskResult, String>>,
        log_path: PathBuf,
    ) -> Result<String, ToolError> {
        Self::ensure_log_dir(&log_path).await?;

        let mut tasks = self.tasks.lock().await;
        if tasks.contains_key(&task_id) {
            return Err(ToolError::Execution(format!(
                "task id '{task_id}' is already registered"
            )));
        }
        tasks.insert(
            task_id.clone(),
            Arc::new(TaskEntry {
                state: Arc::new(Mutex::new(TaskState::Running(handle))),
                log_path,
            }),
        );
        Ok(task_id)
    }

    async fn wait(
        &self,
        task_ids: &[String],
    ) -> Result<HashMap<String, BackgroundTaskResult>, ToolError> {
        let mut results = HashMap::new();

        for task_id in task_ids {
            let entry = self
                .tasks
                .lock()
                .await
                .get(task_id)
                .cloned()
                .ok_or_else(|| ToolError::Execution(format!("task '{task_id}' not found")))?;

            // Resolve the task under a single lock so concurrent waits do not
            // both try to await the same handle.
            let result = {
                let mut state = entry.state.lock().await;
                match std::mem::replace(
                    &mut *state,
                    TaskState::Completed(Err("task state corrupted while waiting".to_string())),
                ) {
                    TaskState::Completed(result) => {
                        *state = TaskState::Completed(result.clone());
                        result
                    }
                    TaskState::Running(handle) => {
                        // Release the lock before awaiting the handle.
                        drop(state);
                        let resolved = handle
                            .await
                            .unwrap_or_else(|e| Err(format!("task panicked: {e}")));
                        let mut state = entry.state.lock().await;
                        *state = TaskState::Completed(resolved.clone());
                        resolved
                    }
                }
            };

            let result = result.map_err(ToolError::Execution)?;
            results.insert(task_id.clone(), result);
        }

        Ok(results)
    }

    async fn kill(&self, task_id: &str) -> Result<(), ToolError> {
        let entry = self
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| ToolError::Execution(format!("task '{task_id}' not found")))?;

        let mut state = entry.state.lock().await;
        match std::mem::replace(
            &mut *state,
            TaskState::Completed(Err("task state corrupted while killing".to_string())),
        ) {
            TaskState::Running(handle) => {
                handle.abort();
                *state = TaskState::Completed(Err("task killed".to_string()));
                Ok(())
            }
            TaskState::Completed(result) => {
                *state = TaskState::Completed(result);
                Err(ToolError::Execution(format!(
                    "task '{task_id}' is not running"
                )))
            }
        }
    }

    async fn output(&self, task_id: &str) -> Result<BackgroundTaskOutput, ToolError> {
        let entry = self
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| ToolError::Execution(format!("task '{task_id}' not found")))?;

        let (exit_code, is_running) = match &*entry.state.lock().await {
            TaskState::Running(_) => (None, true),
            TaskState::Completed(Ok(result)) => (Some(result.exit_code), false),
            TaskState::Completed(Err(_)) => (None, false),
        };

        let log_content = tokio::fs::read_to_string(&entry.log_path)
            .await
            .unwrap_or_default();
        let (stdout, stderr) = parse_log(&log_content);

        Ok(BackgroundTaskOutput {
            exit_code,
            stdout,
            stderr,
            is_running,
        })
    }
}

/// Wait for one or more background tasks to complete and return their outputs.
pub struct WaitTasksTool {
    policy: Policy,
}

impl WaitTasksTool {
    pub fn new() -> Self {
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

impl Default for WaitTasksTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WaitTasksTool {
    fn name(&self) -> &str {
        "wait_tasks"
    }

    fn description(&self) -> &str {
        "Wait for background tasks to complete and return their outputs."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "task ids to wait for"
                }
            },
            "required": ["task_ids"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let task_ids = params["task_ids"].as_array().ok_or_else(|| {
            ToolError::InvalidParams("'task_ids' must be an array of strings".to_string())
        })?;
        let task_ids: Vec<String> = task_ids
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();

        let registry = ctx.background_tasks.ok_or_else(|| {
            ToolError::Execution("background task registry is not available".to_string())
        })?;

        let results = registry.wait(&task_ids).await?;
        let content: serde_json::Map<String, serde_json::Value> = results
            .into_iter()
            .map(|(id, result)| {
                (
                    id,
                    json!({
                        "exit_code": result.exit_code,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                    }),
                )
            })
            .collect();

        Ok(ToolResult::ok(json!(content).to_string()))
    }
}

/// Kill a running background task.
pub struct KillTaskTool {
    policy: Policy,
}

impl KillTaskTool {
    pub fn new() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Required,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

impl Default for KillTaskTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for KillTaskTool {
    fn name(&self) -> &str {
        "kill_task"
    }

    fn description(&self) -> &str {
        "Kill a running background task."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "task id to kill" }
            },
            "required": ["task_id"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let task_id = params["task_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'task_id' parameter".to_string()))?;

        let registry = ctx.background_tasks.ok_or_else(|| {
            ToolError::Execution("background task registry is not available".to_string())
        })?;

        registry.kill(task_id).await?;
        Ok(ToolResult::ok(format!("killed task '{task_id}'")))
    }
}

/// Return the current output of a background task without waiting.
pub struct GetTaskOutputTool {
    policy: Policy,
}

impl GetTaskOutputTool {
    pub fn new() -> Self {
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

impl Default for GetTaskOutputTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GetTaskOutputTool {
    fn name(&self) -> &str {
        "get_task_output"
    }

    fn description(&self) -> &str {
        "Return the current output of a background task without waiting for it to finish."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "task id to query" }
            },
            "required": ["task_id"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let task_id = params["task_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'task_id' parameter".to_string()))?;

        let registry = ctx.background_tasks.ok_or_else(|| {
            ToolError::Execution("background task registry is not available".to_string())
        })?;

        let output = registry.output(task_id).await?;
        let content = json!({
            "exit_code": output.exit_code,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "is_running": output.is_running,
        })
        .to_string();

        Ok(ToolResult::ok(content))
    }
}

/// Write a task's output to its log file.
pub async fn write_task_log(log_path: &Path, stdout: &str, stderr: &str) -> Result<(), ToolError> {
    let content = json!({
        "stdout": stdout,
        "stderr": stderr,
    })
    .to_string();
    tokio::fs::write(log_path, content)
        .await
        .map_err(|e| ToolError::Execution(format!("failed to write task log: {e}")))?;
    Ok(())
}

fn parse_log(content: &str) -> (String, String) {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .map(|v| {
            (
                v["stdout"].as_str().unwrap_or("").to_string(),
                v["stderr"].as_str().unwrap_or("").to_string(),
            )
        })
        .unwrap_or_else(|| (content.to_string(), String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ok_handle(
        result: BackgroundTaskResult,
    ) -> tokio::task::JoinHandle<Result<BackgroundTaskResult, String>> {
        tokio::spawn(async move { Ok(result) })
    }

    #[tokio::test]
    async fn registry_tracks_task_and_returns_output() {
        let registry = BackgroundTaskRegistry::new();
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("task.log");

        let task_id = registry
            .register(
                "t1".to_string(),
                ok_handle(BackgroundTaskResult {
                    exit_code: 0,
                    stdout: "hello".to_string(),
                    stderr: "".to_string(),
                }),
                log_path.clone(),
            )
            .await
            .unwrap();

        assert_eq!(task_id, "t1");

        // Pre-write log before waiting.
        write_task_log(&log_path, "hello", "").await.unwrap();

        let output = registry.output(&task_id).await.unwrap();
        assert!(output.is_running);
        assert_eq!(output.stdout, "hello");

        let results = registry.wait(std::slice::from_ref(&task_id)).await.unwrap();
        assert_eq!(results[&task_id].exit_code, 0);

        let output = registry.output(&task_id).await.unwrap();
        assert!(!output.is_running);
        assert_eq!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn kill_aborts_running_task() {
        let registry = BackgroundTaskRegistry::new();
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("task.log");

        let handle: tokio::task::JoinHandle<Result<BackgroundTaskResult, String>> =
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(BackgroundTaskResult {
                    exit_code: 0,
                    stdout: "".to_string(),
                    stderr: "".to_string(),
                })
            });

        let task_id = registry
            .register("t2".to_string(), handle, log_path)
            .await
            .unwrap();

        registry.kill(&task_id).await.unwrap();

        let output = registry.output(&task_id).await.unwrap();
        assert!(!output.is_running);
        assert!(output.exit_code.is_none());

        // Wait should report the task as failed.
        let err = registry.wait(&[task_id]).await.unwrap_err();
        assert!(err.to_string().contains("killed"));
    }
}
