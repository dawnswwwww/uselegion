use std::sync::Arc;

use async_trait::async_trait;
use legion_runtime::{
    BackgroundTaskResult, Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult,
};
use serde_json::json;

use crate::background_task::{
    BackgroundTaskRegistry as BackgroundTaskRegistryImpl, task_log_path, write_task_log,
};
use crate::policy::Policy;
use crate::sandbox::{ExecResult, LocalSandboxBackend, SandboxBackend};

/// Helper to stamp `kind()` and `namespace()` on a built-in Legion tool.
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

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

/// Execute a shell command and capture its output.
pub struct ExecTool {
    pub policy: Policy,
    backend: Arc<dyn SandboxBackend>,
}

impl ExecTool {
    pub fn new(policy: Policy) -> Self {
        Self::with_backend(policy, Arc::new(LocalSandboxBackend::new()))
    }

    pub fn with_backend(policy: Policy, backend: Arc<dyn SandboxBackend>) -> Self {
        Self { policy, backend }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout, stderr and exit code."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "shell command to execute" },
                "timeout": { "type": "integer", "description": "timeout in seconds (default 60)" },
                "is_background": { "type": "boolean", "description": "when true, spawn the command as a background task and return a task_id handle" }
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    legion_tool_taxonomy!(ToolKind::Execute);

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'command' parameter".to_string()))?;
        let timeout_secs = params["timeout"].as_u64().unwrap_or(60);
        let is_background = params["is_background"].as_bool().unwrap_or(false);

        if !is_background {
            let result = self
                .backend
                .exec(command, &ctx.workspace, timeout_secs)
                .await
                .map_err(|e| ToolError::Execution(format!("sandbox exec failed: {}", e)))?;

            let content = json!({
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr,
            })
            .to_string();

            return Ok(ToolResult {
                content,
                is_error: result.exit_code != 0,
            });
        }

        let registry = ctx.background_tasks.ok_or_else(|| {
            ToolError::Execution("background task registry is not available".to_string())
        })?;

        let task_id = registry.next_task_id();
        let log_path = task_log_path(&ctx.session_id, &task_id);
        BackgroundTaskRegistryImpl::ensure_log_dir(&log_path).await?;

        let backend = self.backend.clone();
        let command = command.to_string();
        let cwd = ctx.workspace.clone();
        let log_path_for_task = log_path.clone();
        let handle = tokio::spawn(async move {
            let result = backend.exec(&command, &cwd, timeout_secs).await;
            match result {
                Ok(exec_result) => {
                    if let Err(e) =
                        write_task_log(&log_path_for_task, &exec_result.stdout, &exec_result.stderr)
                            .await
                    {
                        return Err(format!("failed to write task log: {e}"));
                    }
                    Ok(BackgroundTaskResult {
                        exit_code: exec_result.exit_code,
                        stdout: exec_result.stdout,
                        stderr: exec_result.stderr,
                    })
                }
                Err(e) => Err(format!("sandbox exec failed: {e}")),
            }
        });

        let registered_id = registry.register(task_id, handle, log_path).await?;

        Ok(ToolResult::ok(
            json!({ "task_id": registered_id }).to_string(),
        ))
    }
}

impl From<ExecResult> for ToolResult {
    fn from(result: ExecResult) -> Self {
        let content = json!({
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
        })
        .to_string();
        ToolResult {
            content,
            is_error: result.exit_code != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_task::{KillTaskTool, WaitTasksTool};
    use legion_runtime::BackgroundTaskRegistry as _;
    use tempfile::TempDir;

    fn ctx(dir: &TempDir, sender: Option<&str>) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            sender: sender.map(|s| s.to_string()),
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

    fn ctx_with_background_tasks(
        dir: &TempDir,
        registry: Arc<crate::background_task::BackgroundTaskRegistry>,
    ) -> ToolContext {
        // Use a unique session id derived from the temp directory name so
        // concurrent background-task tests do not collide on the same log path.
        let session_id = dir
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "s1".to_string());
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id,
            agent_id: "a1".to_string(),
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
            background_tasks: Some(registry),
            plan_mode_tracker: None,
        }
    }

    fn open_policy() -> Policy {
        Policy {
            approval: crate::policy::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    #[tokio::test]
    async fn exec_returns_output() {
        let dir = TempDir::new().unwrap();
        let tool = ExecTool::new(open_policy());
        let res = tool
            .execute(
                json!({"command": "echo hello && echo err >&2 && exit 42"}),
                ctx(&dir, None),
            )
            .await
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&res.content).unwrap();
        assert!(parsed["stdout"].as_str().unwrap().contains("hello"));
        assert!(parsed["stderr"].as_str().unwrap().contains("err"));
        assert_eq!(parsed["exit_code"].as_i64(), Some(42));
        assert!(res.is_error);
    }

    #[test]
    fn exec_tool_is_not_concurrency_safe() {
        let tool = ExecTool::new(open_policy());
        let input = json!({"command": "echo hi"});
        assert!(!tool.is_concurrency_safe(&input));
        assert!(!tool.is_read_only(&input));
    }

    // -----------------------------------------------------------------------
    // background task tools
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn exec_background_returns_task_id_and_log_is_written() {
        let dir = TempDir::new().unwrap();
        let registry = Arc::new(crate::background_task::BackgroundTaskRegistry::new());
        let tool = ExecTool::new(open_policy());

        let res = tool
            .execute(
                json!({"command": "echo bg-out && echo bg-err >&2", "is_background": true}),
                ctx_with_background_tasks(&dir, registry.clone()),
            )
            .await
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&res.content).unwrap();
        let task_id = parsed["task_id"].as_str().unwrap();
        assert!(task_id.starts_with("task-"));

        let output = registry.output(task_id).await.unwrap();
        assert!(output.is_running);

        // Wait for the task to finish and verify the log.
        let _ = registry.wait(&[task_id.to_string()]).await.unwrap();
        let output = registry.output(task_id).await.unwrap();
        assert!(!output.is_running);
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("bg-out"));
        assert!(output.stderr.contains("bg-err"));
    }

    #[tokio::test]
    async fn wait_tasks_returns_task_outputs() {
        let dir = TempDir::new().unwrap();
        let registry = Arc::new(crate::background_task::BackgroundTaskRegistry::new());
        let exec = ExecTool::new(open_policy());

        let res = exec
            .execute(
                json!({"command": "echo one", "is_background": true}),
                ctx_with_background_tasks(&dir, registry.clone()),
            )
            .await
            .unwrap();
        let task_id = serde_json::from_str::<serde_json::Value>(&res.content).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_string();

        let wait = WaitTasksTool::new();
        let res = wait
            .execute(
                json!({"task_ids": [task_id.clone()]}),
                ctx_with_background_tasks(&dir, registry.clone()),
            )
            .await
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&res.content).unwrap();
        assert_eq!(parsed[task_id.clone()]["exit_code"].as_i64(), Some(0));
        assert!(parsed[task_id]["stdout"].as_str().unwrap().contains("one"));
    }

    #[tokio::test]
    async fn kill_task_aborts_background_task() {
        let dir = TempDir::new().unwrap();
        let registry = Arc::new(crate::background_task::BackgroundTaskRegistry::new());
        let exec = ExecTool::new(open_policy());

        let res = exec
            .execute(
                json!({"command": "sleep 60", "is_background": true}),
                ctx_with_background_tasks(&dir, registry.clone()),
            )
            .await
            .unwrap();
        let task_id = serde_json::from_str::<serde_json::Value>(&res.content).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_string();

        let output = registry.output(&task_id).await.unwrap();
        assert!(output.is_running);

        let kill = KillTaskTool::new();
        let res = kill
            .execute(
                json!({"task_id": task_id.clone()}),
                ctx_with_background_tasks(&dir, registry.clone()),
            )
            .await
            .unwrap();
        assert!(res.content.contains("killed"));

        let output = registry.output(&task_id).await.unwrap();
        assert!(!output.is_running);
    }
}
