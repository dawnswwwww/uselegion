use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures::SinkExt;
use futures::channel::mpsc::Sender;
use legion_provider::types::{ChatMessage, ChatRole, ToolCall as ProviderToolCall};

use crate::approval::{ApprovalCtx, ApprovalRequest, PermissionMode};
use crate::memory::MemoryBackend;
use crate::messenger::AgentMessenger;
use crate::plan_mode::PlanModeTracker;
use crate::question::QuestionCtx;
use crate::subagent::SubagentSpawner;
use crate::swarm::SwarmManager;
use crate::tools::{
    CanUseToolFn, Permission, Tool, ToolCall, ToolContext, ToolError, ToolRegistry, ToolResult,
    apply_permission_mode,
};
use crate::types::RunEvent;
use legion_telemetry::{SessionMetric, TelemetryClient};

/// A batch of tool calls that share the same concurrency mode.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolBatch {
    /// `true` when every call in this batch may execute concurrently.
    pub concurrent: bool,
    /// Tool calls in invocation order.
    pub calls: Vec<ToolCall>,
}

/// Partition a flat list of tool calls into concurrent and sequential batches.
///
/// The algorithm walks the calls in order. Consecutive calls whose tool
/// declares `is_concurrency_safe(input) == true` are grouped into a single
/// concurrent batch. Any call that is not concurrency-safe (including unknown
/// tools) ends the current concurrent batch and becomes its own sequential
/// batch.
///
/// This matches Claude Code's `partitionToolCalls`: a write operation forces
/// everything after it to wait until it completes.
pub fn partition_tool_calls(registry: &dyn ToolRegistry, calls: &[ToolCall]) -> Vec<ToolBatch> {
    let mut batches: Vec<ToolBatch> = Vec::new();
    let mut current_concurrent: Vec<ToolCall> = Vec::new();

    for call in calls {
        let tool_opt = registry.get(&call.name);
        let arguments =
            serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap_or_default();
        let is_safe = tool_opt
            .as_ref()
            .map(|t| t.is_concurrency_safe(&arguments))
            .unwrap_or(false);

        if is_safe {
            current_concurrent.push(call.clone());
        } else {
            if !current_concurrent.is_empty() {
                batches.push(ToolBatch {
                    concurrent: true,
                    calls: std::mem::take(&mut current_concurrent),
                });
            }
            batches.push(ToolBatch {
                concurrent: false,
                calls: vec![call.clone()],
            });
        }
    }

    if !current_concurrent.is_empty() {
        batches.push(ToolBatch {
            concurrent: true,
            calls: std::mem::take(&mut current_concurrent),
        });
    }

    batches
}

/// Validate a tool invocation against its declared JSON Schema and any
/// additional semantic validation implemented by the tool.
pub fn validate_tool_input(tool: &dyn Tool, input: &serde_json::Value) -> Result<(), ToolError> {
    validate_schema(&tool.schema(), input)?;
    tool.validate_input(input)
}

fn validate_schema(schema: &serde_json::Value, input: &serde_json::Value) -> Result<(), ToolError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| ToolError::InvalidParams(format!("invalid tool schema: {e}")))?;
    if let Err(error) = validator.validate(input) {
        return Err(ToolError::InvalidParams(format!(
            "schema validation failed: {error}"
        )));
    }
    Ok(())
}

/// Execute a single tool call through the full runtime pipeline:
///
/// 1. Parse arguments.
/// 2. JSON Schema + semantic validation.
/// 3. Permission check via `can_use_tool`.
/// 4. Tool execution.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_call(
    call: &ToolCall,
    workspace: &Path,
    session_id: &str,
    agent_id: &str,
    sender: Option<&str>,
    registry: &dyn ToolRegistry,
    can_use_tool: Option<&CanUseToolFn>,
    memory_backend: Option<Arc<dyn MemoryBackend>>,
    viewed_files: Option<Arc<std::sync::Mutex<HashSet<PathBuf>>>>,
    approval: Option<ApprovalCtx>,
    question: Option<QuestionCtx>,
    allowed_tools: Option<Vec<String>>,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    messenger: Option<Arc<dyn AgentMessenger>>,
    swarm: Option<Arc<SwarmManager>>,
    depth: u8,
    parent_history: Option<Arc<Vec<ChatMessage>>>,
    todo_store: Option<crate::SharedTodoStore>,
    background_tasks: Option<Arc<dyn crate::tools::BackgroundTaskRegistry>>,
    plan_mode_tracker: Option<Arc<tokio::sync::Mutex<PlanModeTracker>>>,
) -> ToolResult {
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            return ToolResult::error(format!("tool '{}' not found", call.name));
        }
    };

    let input = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
        Ok(p) => p,
        Err(err) => {
            return ToolResult::error(format!("invalid tool arguments: {err}"));
        }
    };

    if let Err(err) = validate_tool_input(tool.as_ref(), &input) {
        return ToolResult::error(err.to_string());
    }

    if let Some(decider) = can_use_tool {
        let permission = decider(tool.name(), &input, sender).await;
        let mode = approval
            .as_ref()
            .map(|ctx| ctx.permission_mode)
            .unwrap_or(PermissionMode::Default);
        let permission = apply_permission_mode(mode, permission, tool.as_ref(), &input);
        match permission {
            Permission::Allow => {}
            Permission::Prompt { message } => match approval {
                Some(ctx) => {
                    let req = ApprovalRequest {
                        tool: tool.name().to_string(),
                        agent_id: agent_id.to_string(),
                        session_key: session_id.to_string(),
                        interactive: ctx.interactive,
                    };
                    if !ctx.gate.request(&req).await {
                        return ToolResult::error(format!(
                            "tool '{}' approval denied",
                            tool.name()
                        ));
                    }
                }
                None => {
                    return ToolResult::error(format!(
                        "tool '{}' needs approval: {message}",
                        tool.name()
                    ));
                }
            },
            Permission::Deny { reason } => {
                return ToolResult::error(format!(
                    "tool '{}' is not allowed: {reason}",
                    tool.name()
                ));
            }
        }
    }

    // Plan-mode enforcement: when active, mutating tools must target the plan
    // file. Read-only tools pass through. This applies regardless of the
    // session permission mode so that plan mode remains a hard boundary.
    if let Some(tracker) = &plan_mode_tracker {
        let guard = tracker.lock().await;
        if guard.is_active() {
            let restricted = matches!(tool.name(), "write" | "edit" | "apply_patch" | "exec");
            if restricted {
                let allowed = match tool.name() {
                    "write" | "edit" | "apply_patch" => {
                        input.get("path").and_then(|v| v.as_str()).is_some_and(|p| {
                            let resolved = if Path::new(p).is_absolute() {
                                PathBuf::from(p)
                            } else {
                                workspace.join(p)
                            };
                            guard.should_auto_approve_edit(&resolved)
                        })
                    }
                    "exec" => false,
                    _ => false,
                };
                if !allowed {
                    return ToolResult::error(format!(
                        "tool '{}' is blocked in plan mode except when targeting the plan file ({})",
                        tool.name(),
                        guard.plan_file_path().display()
                    ));
                }
            }
        }
    }

    let ctx = ToolContext {
        workspace: workspace.to_path_buf(),
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        sender: sender.map(|s| s.to_string()),
        memory: memory_backend,
        viewed_files,
        allowed_tools,
        spawner,
        messenger,
        swarm,
        depth,
        parent_history,
        question_gate: question.map(|q| q.gate),
        todo_store,
        background_tasks,
        plan_mode_tracker,
    };

    match tool.execute(input, ctx).await {
        Ok(res) => res,
        Err(err) => ToolResult::error(err.to_string()),
    }
}

/// Run a sequence of tool batches, emitting start/end events and returning
/// the resulting `ChatMessage::Tool` messages in the order expected by the
/// provider APIs.
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_batches(
    batches: Vec<ToolBatch>,
    workspace: &Path,
    session_id: &str,
    agent_id: &str,
    sender: Option<&str>,
    registry: &Arc<dyn ToolRegistry>,
    can_use_tool: Option<&CanUseToolFn>,
    memory_backend: Option<Arc<dyn MemoryBackend>>,
    viewed_files: Option<Arc<std::sync::Mutex<HashSet<PathBuf>>>>,
    approval: Option<ApprovalCtx>,
    question: Option<QuestionCtx>,
    allowed_tools: Option<Vec<String>>,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    messenger: Option<Arc<dyn AgentMessenger>>,
    swarm: Option<Arc<SwarmManager>>,
    depth: u8,
    parent_history: Option<Arc<Vec<ChatMessage>>>,
    todo_store: Option<crate::SharedTodoStore>,
    background_tasks: Option<Arc<dyn crate::tools::BackgroundTaskRegistry>>,
    plan_mode_tracker: Option<Arc<tokio::sync::Mutex<PlanModeTracker>>>,
    telemetry: Option<Arc<TelemetryClient>>,
    turn_number: usize,
    tx: &mut Sender<RunEvent>,
) -> Vec<ChatMessage> {
    let mut results = Vec::new();

    for batch in batches {
        if batch.concurrent {
            // Execute all calls concurrently, preserving invocation order in
            // the returned messages.
            let mut handles = Vec::with_capacity(batch.calls.len());
            for call in &batch.calls {
                let call = call.clone();
                let workspace = workspace.to_path_buf();
                let session_id = session_id.to_string();
                let agent_id = agent_id.to_string();
                let sender = sender.map(|s| s.to_string());
                let registry = registry.clone();
                let can_use_tool = can_use_tool.cloned();
                let memory_backend = memory_backend.clone();
                let viewed_files = viewed_files.clone();
                let approval = approval.clone();
                let question = question.clone();
                let allowed_tools = allowed_tools.clone();
                let spawner = spawner.clone();
                let messenger = messenger.clone();
                let swarm = swarm.clone();
                let parent_history = parent_history.clone();
                let todo_store = todo_store.clone();
                let background_tasks = background_tasks.clone();
                let plan_mode_tracker = plan_mode_tracker.clone();
                let input =
                    serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap_or_default();
                let read_only = registry
                    .get(&call.name)
                    .map(|t| t.is_read_only(&input))
                    .unwrap_or(false);
                let start = Instant::now();

                handles.push(tokio::spawn(async move {
                    let result = execute_tool_call(
                        &call,
                        &workspace,
                        &session_id,
                        &agent_id,
                        sender.as_deref(),
                        registry.as_ref(),
                        can_use_tool.as_ref(),
                        memory_backend,
                        viewed_files,
                        approval,
                        question,
                        allowed_tools,
                        spawner,
                        messenger,
                        swarm,
                        depth,
                        parent_history,
                        todo_store,
                        background_tasks,
                        plan_mode_tracker,
                    )
                    .await;
                    (result, start.elapsed().as_millis() as u64, read_only)
                }));
            }

            for (call, handle) in batch.calls.iter().zip(handles) {
                send(
                    tx,
                    RunEvent::ToolStart {
                        tool_call: call.clone(),
                    },
                )
                .await;

                let (result, duration_ms, read_only) = match handle.await {
                    Ok(res) => res,
                    Err(join_err) => (
                        ToolResult::error(format!("tool task panicked: {join_err}")),
                        0,
                        false,
                    ),
                };

                log_tool_call(
                    &telemetry,
                    session_id,
                    turn_number,
                    &call.name,
                    read_only,
                    duration_ms,
                )
                .await;

                let input =
                    serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap_or_default();
                let canonical_meta = registry.get(&call.name).map(|t| t.canonical_meta(&input));

                send(
                    tx,
                    RunEvent::ToolEnd {
                        tool_call: call.clone(),
                        result: result.clone(),
                        canonical_meta,
                    },
                )
                .await;

                results.push(tool_result_message(&call.id, &result));
            }
        } else {
            for call in batch.calls {
                send(
                    tx,
                    RunEvent::ToolStart {
                        tool_call: call.clone(),
                    },
                )
                .await;

                let input =
                    serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap_or_default();
                let read_only = registry
                    .get(&call.name)
                    .map(|t| t.is_read_only(&input))
                    .unwrap_or(false);
                let start = Instant::now();
                let result = execute_tool_call(
                    &call,
                    workspace,
                    session_id,
                    agent_id,
                    sender,
                    registry.as_ref(),
                    can_use_tool,
                    memory_backend.clone(),
                    viewed_files.clone(),
                    approval.clone(),
                    question.clone(),
                    allowed_tools.clone(),
                    spawner.clone(),
                    messenger.clone(),
                    swarm.clone(),
                    depth,
                    parent_history.clone(),
                    todo_store.clone(),
                    background_tasks.clone(),
                    plan_mode_tracker.clone(),
                )
                .await;
                let duration_ms = start.elapsed().as_millis() as u64;

                log_tool_call(
                    &telemetry,
                    session_id,
                    turn_number,
                    &call.name,
                    read_only,
                    duration_ms,
                )
                .await;

                let canonical_meta = registry.get(&call.name).map(|t| t.canonical_meta(&input));

                send(
                    tx,
                    RunEvent::ToolEnd {
                        tool_call: call.clone(),
                        result: result.clone(),
                        canonical_meta,
                    },
                )
                .await;

                results.push(tool_result_message(&call.id, &result));
            }
        }
    }

    results
}

fn tool_result_message(tool_call_id: &str, result: &ToolResult) -> ChatMessage {
    ChatMessage {
        role: ChatRole::Tool,
        content: result.content.clone(),
        name: None,
        tool_calls: None,
        tool_call_id: Some(tool_call_id.to_string()),
        cache_breakpoint: false,
    }
}

async fn send(tx: &mut Sender<RunEvent>, event: RunEvent) {
    let _ = tx.send(event).await;
}

async fn log_tool_call(
    telemetry: &Option<Arc<TelemetryClient>>,
    session_id: &str,
    turn_number: usize,
    tool: &str,
    read_only: bool,
    duration_ms: u64,
) {
    if let Some(telemetry) = telemetry {
        telemetry
            .log_session_event(SessionMetric::ToolCalled {
                session_id: session_id.to_string(),
                turn_number,
                tool: tool.to_string(),
                read_only,
                duration_ms,
            })
            .await;
    }
}

impl ToolCall {
    /// Build a runtime `ToolCall` from a provider tool call.
    pub fn from_provider(tc: &ProviderToolCall) -> Self {
        Self {
            id: tc.id.clone(),
            name: tc.function.name.clone(),
            arguments: tc.function.arguments.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalGate, NoOpApprovalNotifier};
    use crate::plan_mode::PlanModeTracker;
    use crate::tools::{Approval, Policy, ToolDefinitionExt, build_policy_decider};
    use async_trait::async_trait;
    use futures::StreamExt;
    use legion_provider::types::ToolDefinition;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex;

    fn open_policy() -> &'static Policy {
        use std::sync::OnceLock;
        static POLICY: OnceLock<Policy> = OnceLock::new();
        POLICY.get_or_init(|| Policy {
            approval: Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }

    struct FakeReadTool;

    #[async_trait]
    impl Tool for FakeReadTool {
        fn name(&self) -> &str {
            "read"
        }

        fn description(&self) -> &str {
            "read"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            })
        }

        fn policy(&self) -> &Policy {
            open_policy()
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
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let path = params["path"].as_str().unwrap_or("");
            Ok(ToolResult::ok(format!("read {path}")))
        }
    }

    struct FakeWriteTool;

    #[async_trait]
    impl Tool for FakeWriteTool {
        fn name(&self) -> &str {
            "write"
        }

        fn description(&self) -> &str {
            "write"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            })
        }

        fn policy(&self) -> &Policy {
            open_policy()
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
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let path = params["path"].as_str().unwrap_or("");
            Ok(ToolResult::ok(format!("wrote {path}")))
        }
    }

    struct FakeEchoTool;

    #[async_trait]
    impl Tool for FakeEchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "echo"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "msg": { "type": "string" }
                },
                "required": ["msg"]
            })
        }

        fn policy(&self) -> &Policy {
            open_policy()
        }

        async fn execute(
            &self,
            params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok(
                params["msg"].as_str().unwrap_or("").to_string(),
            ))
        }
    }

    struct FakeToolRegistry {
        tools: HashMap<String, Arc<dyn Tool>>,
    }

    impl FakeToolRegistry {
        fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
            let mut map = HashMap::new();
            for t in tools {
                map.insert(t.name().to_string(), t);
            }
            Self { tools: map }
        }
    }

    #[async_trait]
    impl ToolRegistry for FakeToolRegistry {
        fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
            self.tools.get(name).cloned()
        }

        fn definitions(&self) -> Vec<ToolDefinition> {
            self.tools.values().map(|t| t.definition()).collect()
        }
    }

    fn registry() -> Arc<dyn ToolRegistry> {
        Arc::new(FakeToolRegistry::new(vec![
            Arc::new(FakeReadTool),
            Arc::new(FakeWriteTool),
            Arc::new(FakeEchoTool),
        ]))
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[test]
    fn partition_two_reads_into_concurrent_batch() {
        let registry = registry();
        let calls = vec![
            call("c1", "read", r#"{"path":"a"}"#),
            call("c2", "read", r#"{"path":"b"}"#),
        ];
        let batches = partition_tool_calls(registry.as_ref(), &calls);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].concurrent);
        assert_eq!(batches[0].calls.len(), 2);
    }

    #[test]
    fn partition_read_then_write_splits_batches() {
        let registry = registry();
        let calls = vec![
            call("c1", "read", r#"{"path":"a"}"#),
            call("c2", "write", r#"{"path":"b","content":"x"}"#),
        ];
        let batches = partition_tool_calls(registry.as_ref(), &calls);
        assert_eq!(batches.len(), 2);
        assert!(batches[0].concurrent);
        assert_eq!(batches[0].calls.len(), 1);
        assert!(!batches[1].concurrent);
        assert_eq!(batches[1].calls.len(), 1);
    }

    #[test]
    fn partition_mixed_sequence_is_correct() {
        // [read, read, write, read] -> concurrent[read,read], seq[write], seq[read]
        let registry = registry();
        let calls = vec![
            call("c1", "read", r#"{"path":"a"}"#),
            call("c2", "read", r#"{"path":"b"}"#),
            call("c3", "write", r#"{"path":"c","content":"x"}"#),
            call("c4", "read", r#"{"path":"d"}"#),
        ];
        let batches = partition_tool_calls(registry.as_ref(), &calls);
        assert_eq!(batches.len(), 3);
        assert!(batches[0].concurrent);
        assert_eq!(batches[0].calls.len(), 2);
        assert!(!batches[1].concurrent);
        assert_eq!(batches[1].calls[0].id, "c3");
        assert!(batches[2].concurrent);
        assert_eq!(batches[2].calls[0].id, "c4");
    }

    #[test]
    fn partition_unknown_tool_is_sequential() {
        let registry = registry();
        let calls = vec![call("c1", "unknown", "{}")];
        let batches = partition_tool_calls(registry.as_ref(), &calls);
        assert_eq!(batches.len(), 1);
        assert!(!batches[0].concurrent);
    }

    #[test]
    fn partition_empty() {
        let registry = registry();
        let batches = partition_tool_calls(registry.as_ref(), &[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn schema_validation_accepts_valid_input() {
        let echo = FakeEchoTool;
        let input = serde_json::json!({"msg": "hi"});
        assert!(validate_tool_input(&echo, &input).is_ok());
    }

    #[test]
    fn schema_validation_rejects_missing_required() {
        let echo = FakeEchoTool;
        let input = serde_json::json!({});
        let err = validate_tool_input(&echo, &input).unwrap_err();
        assert!(err.to_string().contains("msg"));
    }

    #[test]
    fn schema_validation_rejects_wrong_type() {
        let echo = FakeEchoTool;
        let input = serde_json::json!({"msg": 123});
        let err = validate_tool_input(&echo, &input).unwrap_err();
        assert!(err.to_string().contains("schema validation failed"));
    }

    #[test]
    fn semantic_validation_runs_after_schema() {
        struct AlwaysInvalidTool;
        #[async_trait]
        impl Tool for AlwaysInvalidTool {
            fn name(&self) -> &str {
                "invalid"
            }
            fn description(&self) -> &str {
                "invalid"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}})
            }
            fn policy(&self) -> &Policy {
                open_policy()
            }
            fn validate_input(&self, _input: &serde_json::Value) -> Result<(), ToolError> {
                Err(ToolError::InvalidParams("semantic error".to_string()))
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _ctx: ToolContext,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::ok(""))
            }
        }

        let tool = AlwaysInvalidTool;
        let err = validate_tool_input(&tool, &serde_json::json!({"x": "ok"})).unwrap_err();
        assert!(err.to_string().contains("semantic error"));
    }

    #[tokio::test]
    async fn permission_deny_blocks_execution() {
        let registry = registry();
        let can_use_tool: CanUseToolFn = Arc::new(|_name, _input, _sender| {
            Box::pin(async move {
                Permission::Deny {
                    reason: "not today".to_string(),
                }
            })
        });
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"a"}"#.to_string(),
        };
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("not today"));
    }

    #[tokio::test]
    async fn async_decider_prompt_fails_closed_without_gate() {
        let registry = registry();
        let can_use_tool: CanUseToolFn = Arc::new(|_name, _input, _sender| {
            Box::pin(async move {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"a"}"#.to_string(),
        };
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("needs approval"));
    }

    struct CapturingNotifier {
        ids: Mutex<tokio::sync::mpsc::UnboundedSender<String>>,
    }

    #[async_trait]
    impl crate::approval::ApprovalNotifier for CapturingNotifier {
        async fn notify(&self, _req: &ApprovalRequest, prompt_id: &str) {
            let _ = self.ids.lock().await.send(prompt_id.to_string());
        }
    }

    #[tokio::test]
    async fn prompt_with_gate_unattended_denies() {
        let registry = registry();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(CapturingNotifier {
                ids: Mutex::new(tx),
            }),
            Duration::from_secs(5),
        ));
        let can_use_tool: CanUseToolFn = Arc::new(|_n, _i, _s| {
            Box::pin(async {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = call("c1", "read", r#"{"path":"a"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            Some(ApprovalCtx {
                gate,
                interactive: false,
                permission_mode: PermissionMode::Default,
            }),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("approval denied"));
    }

    #[tokio::test]
    async fn prompt_with_gate_approve_executes_tool() {
        let registry = registry();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(CapturingNotifier {
                ids: Mutex::new(tx),
            }),
            Duration::from_secs(5),
        ));
        let can_use_tool: CanUseToolFn = Arc::new(|_n, _i, _s| {
            Box::pin(async {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = call("c1", "read", r#"{"path":"a"}"#);
        let gate_for_resolve = gate.clone();
        let handle = tokio::spawn(async move {
            execute_tool_call(
                &call,
                Path::new("/tmp"),
                "s1",
                "a1",
                None,
                registry.as_ref(),
                Some(&can_use_tool),
                None,
                None,
                Some(ApprovalCtx {
                    gate,
                    interactive: true,
                    permission_mode: PermissionMode::Default,
                }),
                None,
                None,
                None,
                None,
                None,
                0,
                None,
                None,
                None,
                None,
            )
            .await
        });
        let prompt_id = rx.recv().await.expect("notifier should fire");
        gate_for_resolve.resolve(&prompt_id, true).await;
        let result = handle.await.unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("read a"));
    }

    #[tokio::test]
    async fn session_mode_bypass_permissions_executes_without_prompt() {
        let registry = registry();
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(NoOpApprovalNotifier),
            Duration::from_secs(5),
        ));
        let can_use_tool: CanUseToolFn = Arc::new(|_n, _i, _s| {
            Box::pin(async {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = call("c1", "read", r#"{"path":"a"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            Some(ApprovalCtx {
                gate,
                interactive: false,
                permission_mode: PermissionMode::BypassPermissions,
            }),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("read a"));
    }

    #[tokio::test]
    async fn session_mode_dont_ask_denies_without_prompt() {
        let registry = registry();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(CapturingNotifier {
                ids: Mutex::new(tx),
            }),
            Duration::from_secs(5),
        ));
        let can_use_tool: CanUseToolFn = Arc::new(|_n, _i, _s| {
            Box::pin(async {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = call("c1", "read", r#"{"path":"a"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            Some(ApprovalCtx {
                gate,
                interactive: true,
                permission_mode: PermissionMode::DontAsk,
            }),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("dontAsk"),
            "unexpected error: {}",
            result.content
        );
        // No prompt should have been fired.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn session_mode_auto_allows_read_only_tool() {
        let registry = registry();
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(NoOpApprovalNotifier),
            Duration::from_secs(5),
        ));
        let can_use_tool: CanUseToolFn = Arc::new(|_n, _i, _s| {
            Box::pin(async {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = call("c1", "read", r#"{"path":"a"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            Some(ApprovalCtx {
                gate,
                interactive: false,
                permission_mode: PermissionMode::Auto,
            }),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("read a"));
    }

    #[tokio::test]
    async fn session_mode_auto_prompts_for_mutating_tool() {
        let registry = registry();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(CapturingNotifier {
                ids: Mutex::new(tx),
            }),
            Duration::from_secs(5),
        ));
        let can_use_tool: CanUseToolFn = Arc::new(|_n, _i, _s| {
            Box::pin(async {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = call("c1", "write", r#"{"path":"a","content":"x"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            Some(ApprovalCtx {
                gate,
                interactive: false,
                permission_mode: PermissionMode::Auto,
            }),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        // Unattended prompt must fail closed.
        assert!(result.is_error);
        assert!(result.content.contains("approval denied"));
    }

    #[tokio::test]
    async fn session_mode_accept_edits_allows_write_tool() {
        let registry = registry();
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(NoOpApprovalNotifier),
            Duration::from_secs(5),
        ));
        let can_use_tool: CanUseToolFn = Arc::new(|_n, _i, _s| {
            Box::pin(async {
                Permission::Prompt {
                    message: "needs approval".to_string(),
                }
            })
        });
        let call = call("c1", "write", r#"{"path":"a","content":"x"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            Some(ApprovalCtx {
                gate,
                interactive: false,
                permission_mode: PermissionMode::AcceptEdits,
            }),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("wrote"));
    }

    #[tokio::test]
    async fn per_tool_permission_mode_overrides_approval() {
        // A tool whose policy approval is Required but permissionMode is Auto
        // should be auto-approved when it is read-only.
        struct AutoReadTool;
        #[async_trait]
        impl Tool for AutoReadTool {
            fn name(&self) -> &str {
                "auto_read"
            }
            fn description(&self) -> &str {
                "auto_read"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
            }
            fn policy(&self) -> &Policy {
                use std::sync::OnceLock;
                static POLICY: OnceLock<Policy> = OnceLock::new();
                POLICY.get_or_init(|| Policy {
                    approval: Approval::Required,
                    permission_mode: Some(PermissionMode::Auto),
                    allow_from: vec![],
                    workspace_only: false,
                })
            }
            fn is_read_only(&self, _input: &serde_json::Value) -> bool {
                true
            }
            async fn execute(
                &self,
                params: serde_json::Value,
                _ctx: ToolContext,
            ) -> Result<ToolResult, ToolError> {
                let path = params["path"].as_str().unwrap_or("");
                Ok(ToolResult::ok(format!("read {path}")))
            }
        }

        struct AutoRegistry;
        #[async_trait]
        impl ToolRegistry for AutoRegistry {
            fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
                if name == "auto_read" {
                    Some(Arc::new(AutoReadTool))
                } else {
                    None
                }
            }
            fn definitions(&self) -> Vec<ToolDefinition> {
                vec![]
            }
        }

        let registry: Arc<dyn ToolRegistry> = Arc::new(AutoRegistry);
        let gate = Arc::new(ApprovalGate::new(
            Arc::new(NoOpApprovalNotifier),
            Duration::from_secs(5),
        ));
        let decider = build_policy_decider(registry.clone());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "auto_read".to_string(),
            arguments: r#"{"path":"a"}"#.to_string(),
        };
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&decider),
            None,
            None,
            Some(ApprovalCtx {
                gate,
                interactive: false,
                permission_mode: PermissionMode::Default,
            }),
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("read a"));
    }

    #[tokio::test]
    async fn schema_validation_failure_returns_error_result() {
        let registry = registry();
        let call = ToolCall {
            id: "c1".to_string(),
            name: "echo".to_string(),
            arguments: r#"{}"#.to_string(),
        };
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("msg"));
    }

    #[tokio::test]
    async fn concurrent_batch_runs_all_calls() {
        let registry = registry();
        let batches = vec![ToolBatch {
            concurrent: true,
            calls: vec![
                call("c1", "read", r#"{"path":"a"}"#),
                call("c2", "read", r#"{"path":"b"}"#),
            ],
        }];
        let (mut tx, mut rx) = futures::channel::mpsc::channel(128);
        let messages = run_tool_batches(
            batches,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            &registry,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
            None,
            1,
            &mut tx,
        )
        .await;

        // Drain the channel so the test does not hang.
        drop(tx);
        while rx.next().await.is_some() {}

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tool_call_id, Some("c1".to_string()));
        assert_eq!(messages[1].tool_call_id, Some("c2".to_string()));
    }

    #[tokio::test]
    async fn concurrent_read_is_faster_than_sequential() {
        static SLEEP_COUNTER: AtomicUsize = AtomicUsize::new(0);

        struct SlowReadTool;
        #[async_trait]
        impl Tool for SlowReadTool {
            fn name(&self) -> &str {
                "slow_read"
            }
            fn description(&self) -> &str {
                "slow_read"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                })
            }
            fn policy(&self) -> &Policy {
                open_policy()
            }
            fn is_read_only(&self, _input: &serde_json::Value) -> bool {
                true
            }
            fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
                true
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _ctx: ToolContext,
            ) -> Result<ToolResult, ToolError> {
                SLEEP_COUNTER.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                Ok(ToolResult::ok("ok"))
            }
        }

        let registry: Arc<dyn ToolRegistry> =
            Arc::new(FakeToolRegistry::new(vec![Arc::new(SlowReadTool)]));
        let batches = vec![ToolBatch {
            concurrent: true,
            calls: vec![
                call("c1", "slow_read", r#"{"path":"a"}"#),
                call("c2", "slow_read", r#"{"path":"b"}"#),
            ],
        }];

        let start = std::time::Instant::now();
        let (mut tx, mut rx) = futures::channel::mpsc::channel(128);
        run_tool_batches(
            batches,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            &registry,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
            None,
            1,
            &mut tx,
        )
        .await;
        drop(tx);
        while rx.next().await.is_some() {}
        let elapsed = start.elapsed();

        assert!(
            elapsed < tokio::time::Duration::from_millis(200),
            "concurrent execution took too long: {elapsed:?}"
        );
        assert_eq!(SLEEP_COUNTER.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn plan_mode_blocks_write_to_non_plan_file() {
        let registry = registry();
        let session_dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(tokio::sync::Mutex::new(PlanModeTracker::new(
            session_dir.path(),
        )));
        tracker.lock().await.activate();

        let can_use_tool: CanUseToolFn =
            Arc::new(|_name, _input, _sender| Box::pin(async move { Permission::Allow }));
        let call = call("c1", "write", r#"{"path":"other.md","content":"x"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            Some(tracker),
        )
        .await;

        assert!(result.is_error);
        assert!(result.content.contains("blocked in plan mode"));
    }

    #[tokio::test]
    async fn plan_mode_allows_write_to_plan_file() {
        let session_dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(tokio::sync::Mutex::new(PlanModeTracker::new(
            session_dir.path(),
        )));
        tracker.lock().await.activate();

        let registry = registry();
        let can_use_tool: CanUseToolFn =
            Arc::new(|_name, _input, _sender| Box::pin(async move { Permission::Allow }));
        let plan_path = session_dir.path().join("plan.md");
        let args = format!(
            "{{\"path\":\"{}\",\"content\":\"# plan\"}}",
            plan_path.display()
        );
        let call = call("c1", "write", &args);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            Some(tracker),
        )
        .await;

        assert!(!result.is_error);
        assert!(result.content.contains("wrote"));
    }

    #[tokio::test]
    async fn plan_mode_allows_read_only_tool() {
        let registry = registry();
        let session_dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(tokio::sync::Mutex::new(PlanModeTracker::new(
            session_dir.path(),
        )));
        tracker.lock().await.activate();

        let can_use_tool: CanUseToolFn =
            Arc::new(|_name, _input, _sender| Box::pin(async move { Permission::Allow }));
        let call = call("c1", "read", r#"{"path":"a"}"#);
        let result = execute_tool_call(
            &call,
            Path::new("/tmp"),
            "s1",
            "a1",
            None,
            registry.as_ref(),
            Some(&can_use_tool),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            Some(tracker),
        )
        .await;

        assert!(!result.is_error);
    }
}
