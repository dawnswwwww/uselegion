use async_trait::async_trait;
use legion_runtime::{
    CoordinatorPlan, SubagentKind, SubagentRequest, SubagentStatus, Tool, ToolContext, ToolError,
    ToolKind, ToolNamespace, ToolResult, run_coordinator_plan,
};
use serde_json::json;

use crate::policy::{Approval, Policy};

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
// spawn_subagent (multi-agent Phase A)
// ---------------------------------------------------------------------------

/// Tool that delegates a sub-task to a child agent and returns its final text
/// as the tool result. The child runs with an isolated context (Typed), a
/// narrowed tool subset, and its own iteration/timeout/depth limits. The
/// spawner is supplied via `ToolContext::spawner`; when absent (e.g. tests or
/// an unwired runtime) the tool reports unavailability.
pub struct SpawnSubagentTool {
    policy: Policy,
}

impl SpawnSubagentTool {
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

impl Default for SpawnSubagentTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn description(&self) -> &str {
        "Delegate a sub-task to a child agent with an isolated context and a \
         narrowed tool subset. Returns the child agent's final text. Use for \
         parallelizable research or focused sub-problems; prefer doing the work \
         directly when the task is small or needs the current conversation \
         context. To run several independent sub-tasks in parallel, issue \
         multiple spawn_subagent calls in the same turn; they execute \
         concurrently, bounded by subagents.maxConcurrent."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["typed", "fork"],
                    "description": "Child kind: 'typed' (default) starts an isolated-context agent addressed by agent_type; 'fork' inherits this run's conversation history, workspace, and router."
                },
                "agent_type": {
                    "type": "string",
                    "description": "Child agent type (an entry in agents.list, or 'main'). Required for kind='typed'; ignored for kind='fork' (the fork always uses the parent agent type)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The task instruction for the child agent."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for the child run; must be a configured alias or provider/model ref. Omit to inherit the default model."
                },
                "allowed_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tool subset for the child; must be within the parent's set. MCP tools (mcp__*) are not passed down. Omit to inherit the parent's effective set."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Optional system prompt injected into the child run."
                },
                "max_iterations": {
                    "type": "integer",
                    "description": "Optional iteration cap override. Omit for no cap (the child is bounded by its wall-clock timeout); subagents.defaultMaxIterations can set a config-wide guard."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional wall-clock timeout override in milliseconds."
                }
            },
            "required": ["prompt"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        // A spawn shares no mutable state with the parent: each child runs on
        // its own tokio task, handle id, and sidechain transcript, bounded by
        // the spawner's `max_concurrent` semaphore. Marking the tool safe lets
        // the model decide per turn whether to run sub-tasks in parallel (by
        // emitting several calls in one turn) or one at a time.
        true
    }

    legion_tool_taxonomy!(ToolKind::Task);

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let kind_str = input
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("typed");
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;

        let (kind, history) = match kind_str {
            "typed" => {
                let agent_type = input
                    .get("agent_type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidParams(
                            "missing 'agent_type' parameter (required for kind='typed')"
                                .to_string(),
                        )
                    })?;
                (SubagentKind::Typed(agent_type.to_string()), Vec::new())
            }
            "fork" => {
                let snapshot = ctx
                    .parent_history
                    .as_ref()
                    .map(|h| h.as_ref().clone())
                    .unwrap_or_default();
                (SubagentKind::Fork, snapshot)
            }
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "unknown sub-agent kind '{other}' (expected 'typed' or 'fork')"
                )));
            }
        };

        // Resolve and validate the child's tool subset (permission narrowing).
        let child_allowed = resolve_child_allowed(&input, ctx.allowed_tools.as_deref())?;

        let model = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let system_prompt = input
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let max_iterations = input
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let timeout = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .map(std::time::Duration::from_millis);

        let spawner = ctx.spawner.ok_or_else(|| {
            ToolError::Execution("sub-agent spawning is not available".to_string())
        })?;

        let req = SubagentRequest {
            kind,
            prompt: prompt.to_string(),
            model,
            allowed_tools: child_allowed,
            parent_agent_id: ctx.agent_id.clone(),
            parent_depth: ctx.depth,
            system_prompt,
            history,
            max_iterations,
            timeout,
        };

        let handle = spawner
            .spawn(req)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let result = handle
            .join()
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(format_result(result))
    }
}

/// Compute the child's allowed-tool subset and enforce permission narrowing:
/// MCP tools are never passed down, and the child's set must be within the
/// parent's effective set (when the parent is itself narrowed).
fn resolve_child_allowed(
    input: &serde_json::Value,
    parent_allowed: Option<&[String]>,
) -> Result<Option<Vec<String>>, ToolError> {
    let child: Option<Vec<String>> = match input.get("allowed_tools") {
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| {
                ToolError::InvalidParams("'allowed_tools' must be an array of strings".to_string())
            })?;
            let mut names = Vec::with_capacity(arr.len());
            for item in arr {
                let name = item.as_str().ok_or_else(|| {
                    ToolError::InvalidParams("'allowed_tools' entries must be strings".to_string())
                })?;
                names.push(name.to_string());
            }
            Some(names)
        }
        // Inherit the parent's effective set when unspecified (None means the
        // parent is the unrestricted root, so the child is also unrestricted).
        None => parent_allowed.map(|p| p.to_vec()),
    };

    if let Some(names) = &child {
        validate_tool_subset(names, parent_allowed)?;
    }

    Ok(child)
}

/// Enforce permission narrowing on a child's tool set: MCP tools are never
/// passed down, and every name must be within the parent's effective set
/// (when the parent is itself narrowed).
fn validate_tool_subset(
    names: &[String],
    parent_allowed: Option<&[String]>,
) -> Result<(), ToolError> {
    for name in names {
        if name.starts_with("mcp__") {
            return Err(ToolError::InvalidParams(format!(
                "MCP tool '{name}' cannot be passed to a sub-agent"
            )));
        }
    }
    if let Some(parent) = parent_allowed {
        for name in names {
            if !parent.iter().any(|p| p == name) {
                return Err(ToolError::InvalidParams(format!(
                    "tool '{name}' is not in the parent agent's allowed set"
                )));
            }
        }
    }
    Ok(())
}

fn format_result(result: legion_runtime::SubagentResult) -> ToolResult {
    let suffix = result
        .transcript_path
        .as_ref()
        .map(|p| format!("\n(transcript: {})", p.display()))
        .unwrap_or_default();
    match result.status {
        SubagentStatus::Completed => ToolResult::ok(format!("{}{}", result.text, suffix)),
        SubagentStatus::Failed(err) => {
            ToolResult::ok(format!("[subagent failed] {err}\n{}{suffix}", result.text))
        }
        SubagentStatus::TimedOut => {
            ToolResult::ok(format!("[subagent timed_out] {}{suffix}", result.text))
        }
        SubagentStatus::Aborted => {
            ToolResult::ok(format!("[subagent aborted] {}{suffix}", result.text))
        }
    }
}

// ---------------------------------------------------------------------------
// agent_to_agent_send (tools-p1p2 Phase B)
// ---------------------------------------------------------------------------

/// Tool that delivers a fire-and-forget message to another agent, triggering
/// an asynchronous turn on the target. The messenger is supplied via
/// `ToolContext::messenger`; when absent (e.g. tests or an unwired runtime)
/// the tool reports unavailability.
///
/// Authorization is not decided here: the messenger enforces the target
/// agent's `allowFrom` list. This tool only rejects the degenerate
/// send-to-self case.
pub struct AgentToAgentSendTool {
    policy: Policy,
}

impl AgentToAgentSendTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for AgentToAgentSendTool {
    fn name(&self) -> &str {
        "agent_to_agent_send"
    }

    fn description(&self) -> &str {
        "Send a message to another agent, triggering an asynchronous turn on \
         that agent. Returns immediately with a delivery confirmation; the \
         target agent's reply is not awaited. Delivery only succeeds when the \
         target agent's allowFrom list includes this agent."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Target agent id (an entry in agents.list)."
                },
                "message": {
                    "type": "string",
                    "description": "The message to deliver to the target agent."
                }
            },
            "required": ["to", "message"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    legion_tool_taxonomy!(ToolKind::Other);

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let to = input
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'to' parameter".to_string()))?;
        let message = input
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'message' parameter".to_string()))?;

        if to == ctx.agent_id {
            return Err(ToolError::Execution("cannot send to self".to_string()));
        }

        let messenger = ctx
            .messenger
            .ok_or_else(|| ToolError::Execution("agent messenger not wired".to_string()))?;

        let confirmation = messenger
            .send(&ctx.agent_id, to, message)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolResult::ok(confirmation))
    }
}

// ---------------------------------------------------------------------------
// run_coordinator (multi-agent Phase C)
// ---------------------------------------------------------------------------

/// Tool that executes a declared multi-phase coordinator plan: tasks within a
/// phase run concurrently as Typed sub-agents; phases run sequentially,
/// gated on their `depends_on` phases. Task prompts may use `{{results}}` to
/// receive the accumulated results of all previously completed phases.
pub struct RunCoordinatorTool {
    policy: Policy,
}

impl RunCoordinatorTool {
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

impl Default for RunCoordinatorTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for RunCoordinatorTool {
    fn name(&self) -> &str {
        "run_coordinator"
    }

    fn description(&self) -> &str {
        "Execute a multi-phase coordinator plan: each phase declares one or more \
         sub-agent tasks (run concurrently), and phases run in declaration order \
         after their `dependsOn` phases complete. Use `{{results}}` in a task \
         prompt to inject the accumulated results of previous phases (e.g. a \
         synthesis phase after parallel research). Prefer spawn_subagent for a \
         single delegation."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "phases": {
                    "type": "array",
                    "description": "Phases in declaration (topological) order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Unique phase name." },
                            "dependsOn": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Names of phases that must complete first (must be declared earlier)."
                            },
                            "tasks": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "agentType": { "type": "string", "description": "Child agent type (agents.list entry, or 'main')." },
                                        "prompt": { "type": "string", "description": "Task instruction; '{{results}}' injects prior phase results." },
                                        "model": { "type": "string" },
                                        "systemPrompt": { "type": "string" },
                                        "allowedTools": { "type": "array", "items": { "type": "string" }, "description": "Per-task tool subset (must be within the parent's set; mcp__* rejected)." },
                                        "maxIterations": { "type": "integer" },
                                        "timeoutMs": { "type": "integer" }
                                    },
                                    "required": ["agentType", "prompt"]
                                }
                            }
                        },
                        "required": ["name", "tasks"]
                    }
                }
            },
            "required": ["phases"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    legion_tool_taxonomy!(ToolKind::Task);

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let plan: CoordinatorPlan = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidParams(format!("invalid coordinator plan: {e}")))?;

        // Permission narrowing applies to every task's tool subset.
        for phase in &plan.phases {
            for task in &phase.tasks {
                if let Some(names) = &task.allowed_tools {
                    validate_tool_subset(names, ctx.allowed_tools.as_deref())?;
                }
            }
        }

        let spawner = ctx.spawner.ok_or_else(|| {
            ToolError::Execution("sub-agent spawning is not available".to_string())
        })?;

        let report = run_coordinator_plan(&plan, &spawner, &ctx.agent_id, ctx.depth)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let task_count: usize = report.phases.iter().map(|p| p.results.len()).sum();
        Ok(ToolResult::ok(format!(
            "[coordinator] {} phase(s), {} task(s)\n{}",
            report.phases.len(),
            task_count,
            report.render()
        )))
    }
}

// ---------------------------------------------------------------------------
// swarm tools (multi-agent Phase D)
// ---------------------------------------------------------------------------

/// Tool that spawns a named, persistent teammate driven by a mailbox. The
/// teammate runs its first turn in the background; later turns are triggered
/// by `swarm_send` deliveries. The swarm manager is supplied via
/// `ToolContext::swarm`; when absent (e.g. tests or an unwired runtime) the
/// tool reports unavailability.
pub struct SwarmSpawnTool {
    policy: Policy,
}

impl SwarmSpawnTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for SwarmSpawnTool {
    fn name(&self) -> &str {
        "swarm_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a named teammate that works on a task in the background and \
         stays alive for follow-up messages via swarm_send. Use for \
         long-running parallel work you want to keep steering; prefer \
         spawn_subagent for one-shot delegation."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Unique teammate name (^[A-Za-z0-9._-]{1,32}$)."
                },
                "prompt": {
                    "type": "string",
                    "description": "Initial task instruction for the teammate."
                },
                "agentType": {
                    "type": "string",
                    "description": "Teammate agent type (an entry in agents.list, or 'main'). Defaults to this agent."
                },
                "allowedTools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tool subset for the teammate; must be within the parent's set. MCP tools (mcp__*) are not passed down. Omit to inherit the parent's effective set."
                }
            },
            "required": ["name", "prompt"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    legion_tool_taxonomy!(ToolKind::Task);

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'name' parameter".to_string()))?;
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;
        let agent_type = input
            .get("agentType")
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.agent_id);

        // `resolve_child_allowed` reads the snake_case key used by
        // spawn_subagent; re-map the camelCase swarm key into the same shape
        // so the shared validation (subset check, mcp__ rejection) applies
        // unchanged.
        let mut resolved_input = input.clone();
        if let Some(tools) = resolved_input.get("allowedTools").cloned() {
            resolved_input["allowed_tools"] = tools;
        }
        let child_allowed = resolve_child_allowed(&resolved_input, ctx.allowed_tools.as_deref())?;

        let swarm = ctx
            .swarm
            .ok_or_else(|| ToolError::Execution("swarm is not available".to_string()))?;

        swarm
            .spawn_teammate(
                name,
                agent_type,
                prompt,
                &ctx.agent_id,
                ctx.depth,
                child_allowed,
            )
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        tracing::info!(teammate = name, agent_type, parent = %ctx.agent_id, "swarm teammate spawned");
        Ok(ToolResult::ok(format!(
            "teammate '{name}' spawned (agent type: {agent_type}); use swarm_send to steer it"
        )))
    }
}

/// Tool that queues a message in a teammate's mailbox, waking it when idle.
pub struct SwarmSendTool {
    policy: Policy,
}

impl SwarmSendTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for SwarmSendTool {
    fn name(&self) -> &str {
        "swarm_send"
    }

    fn description(&self) -> &str {
        "Send a message to a teammate's mailbox. An idle teammate wakes up \
         and runs a new turn with the queued messages; a running teammate \
         picks them up after its current turn. Returns a delivery \
         confirmation immediately."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Target teammate name (as given to swarm_spawn)."
                },
                "message": {
                    "type": "string",
                    "description": "The message to queue for the teammate."
                }
            },
            "required": ["to", "message"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    legion_tool_taxonomy!(ToolKind::Other);

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let to = input
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'to' parameter".to_string()))?;
        let message = input
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'message' parameter".to_string()))?;

        let swarm = ctx
            .swarm
            .ok_or_else(|| ToolError::Execution("swarm is not available".to_string()))?;

        swarm
            .send(&ctx.agent_id, to, message)
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        tracing::info!(from = %ctx.agent_id, to, "swarm message queued");
        Ok(ToolResult::ok(format!(
            "message queued for teammate '{to}'"
        )))
    }
}

/// Tool that reports the status of every teammate in the swarm (read-only).
pub struct SwarmStatusTool {
    policy: Policy,
}

impl SwarmStatusTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for SwarmStatusTool {
    fn name(&self) -> &str {
        "swarm_status"
    }

    fn description(&self) -> &str {
        "Show the status of all teammates: running/idle state, completed \
         turns, mailbox depth, and the latest result of each."
    }

    fn policy(&self) -> &Policy {
        &self.policy
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

    legion_tool_taxonomy!(ToolKind::Other);

    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let swarm = ctx
            .swarm
            .ok_or_else(|| ToolError::Execution("swarm is not available".to_string()))?;

        let infos = swarm.status();
        if infos.is_empty() {
            return Ok(ToolResult::ok("no teammates"));
        }

        let mut out = String::new();
        for (idx, info) in infos.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            let status = match info.status {
                legion_runtime::TeammateStatus::Running => "running",
                legion_runtime::TeammateStatus::Idle => "idle",
            };
            out.push_str(&format!(
                "{} [{}] turns={} mailbox={}",
                info.name, status, info.turns, info.mailbox_depth
            ));
            let last = info
                .last_result
                .as_deref()
                .map(|r| truncate_chars(r, 300))
                .unwrap_or_else(|| "(none)".to_string());
            out.push_str(&format!("\n  last: {last}"));
        }
        Ok(ToolResult::ok(out))
    }
}

/// Truncate to at most `max` chars (char-safe) for status rendering.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
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

    fn open_policy() -> Policy {
        Policy {
            approval: crate::policy::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    #[test]
    fn resolve_child_allowed_inherits_parent_when_unspecified() {
        let parent = vec!["read".to_string(), "write".to_string()];
        let got = resolve_child_allowed(&json!({}), Some(&parent)).unwrap();
        assert_eq!(got, Some(parent));
    }

    #[test]
    fn resolve_child_allowed_unrestricted_root_yields_none() {
        let got = resolve_child_allowed(&json!({}), None).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn resolve_child_allowed_rejects_mcp_tools() {
        let err = resolve_child_allowed(&json!({"allowed_tools": ["mcp__fs__read"]}), None)
            .expect_err("mcp tools must not pass down");
        assert!(err.to_string().contains("MCP tool"));
    }

    #[test]
    fn resolve_child_allowed_rejects_tools_outside_parent_set() {
        let parent = vec!["read".to_string()];
        let err = resolve_child_allowed(&json!({"allowed_tools": ["write"]}), Some(&parent))
            .expect_err("child must be a subset of the parent");
        assert!(err.to_string().contains("not in the parent"));
    }

    #[test]
    fn resolve_child_allowed_accepts_subset_of_parent() {
        let parent = vec!["read".to_string(), "write".to_string()];
        let got =
            resolve_child_allowed(&json!({"allowed_tools": ["read"]}), Some(&parent)).unwrap();
        assert_eq!(got, Some(vec!["read".to_string()]));
    }

    #[tokio::test]
    async fn spawn_subagent_typed_requires_agent_type() {
        let dir = TempDir::new().unwrap();
        let tool = SpawnSubagentTool::new();
        let res = tool.execute(json!({"prompt": "x"}), ctx(&dir, None)).await;
        let err = res.expect_err("typed spawn without agent_type must fail");
        assert!(err.to_string().contains("agent_type"), "got {err}");
    }

    #[tokio::test]
    async fn spawn_subagent_rejects_unknown_kind() {
        let dir = TempDir::new().unwrap();
        let tool = SpawnSubagentTool::new();
        let res = tool
            .execute(json!({"kind": "bogus", "prompt": "x"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("unknown kind must fail");
        assert!(
            err.to_string().contains("unknown sub-agent kind"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn spawn_subagent_fork_does_not_require_agent_type() {
        // Fork parses without agent_type and proceeds until the (absent)
        // spawner is consulted, proving agent_type is not required for forks.
        let dir = TempDir::new().unwrap();
        let tool = SpawnSubagentTool::new();
        let res = tool
            .execute(json!({"kind": "fork", "prompt": "x"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("no spawner wired in this test ctx");
        assert!(
            err.to_string().contains("spawning is not available"),
            "fork should reach the spawner lookup, got {err}"
        );
    }

    #[derive(Default)]
    struct FakeSpawner {
        requests: Mutex<Vec<legion_runtime::SubagentRequest>>,
    }

    #[async_trait]
    impl legion_runtime::SubagentSpawner for FakeSpawner {
        async fn spawn(
            &self,
            req: legion_runtime::SubagentRequest,
        ) -> Result<legion_runtime::SubagentHandle, legion_runtime::SubagentError> {
            self.requests.lock().unwrap().push(req);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(legion_runtime::SubagentResult {
                handle_id: "h".to_string(),
                text: "coordinated-output".to_string(),
                tool_call_count: 0,
                transcript_path: None,
                status: legion_runtime::SubagentStatus::Completed,
            });
            Ok(legion_runtime::SubagentHandle::from_receiver(
                "h".to_string(),
                rx,
            ))
        }
    }

    fn ctx_with_spawner(
        dir: &TempDir,
        spawner: Arc<dyn legion_runtime::SubagentSpawner>,
        allowed_tools: Option<Vec<String>>,
    ) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "main".to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools,
            spawner: Some(spawner),
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

    fn plan_input() -> serde_json::Value {
        json!({
            "phases": [
                {
                    "name": "research",
                    "tasks": [
                        { "agentType": "researcher", "prompt": "gather facts" },
                        { "agentType": "researcher", "prompt": "gather more" }
                    ]
                },
                {
                    "name": "synthesis",
                    "dependsOn": ["research"],
                    "tasks": [
                        { "agentType": "writer", "prompt": "summarize: {{results}}" }
                    ]
                }
            ]
        })
    }

    #[tokio::test]
    async fn run_coordinator_executes_plan_and_returns_report() {
        let dir = TempDir::new().unwrap();
        let spawner = Arc::new(FakeSpawner::default());
        let tool = RunCoordinatorTool::new();
        let res = tool
            .execute(plan_input(), ctx_with_spawner(&dir, spawner.clone(), None))
            .await
            .unwrap();

        assert!(
            res.content.contains("[coordinator] 2 phase(s), 3 task(s)"),
            "got {}",
            res.content
        );
        assert!(res.content.contains("coordinated-output"));

        let reqs = spawner.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert!(
            reqs[2].prompt.contains("coordinated-output"),
            "synthesis prompt must receive accumulated results, got {:?}",
            reqs[2].prompt
        );
        assert!(reqs.iter().all(|r| r.parent_agent_id == "main"));
    }

    #[tokio::test]
    async fn run_coordinator_rejects_task_tools_outside_parent_set() {
        let dir = TempDir::new().unwrap();
        let spawner = Arc::new(FakeSpawner::default());
        let mut plan = plan_input();
        plan["phases"][0]["tasks"][0]["allowedTools"] = json!(["write"]);
        let tool = RunCoordinatorTool::new();
        let res = tool
            .execute(
                plan,
                ctx_with_spawner(&dir, spawner, Some(vec!["read".to_string()])),
            )
            .await;
        let err = res.expect_err("task subset must be within the parent's set");
        assert!(err.to_string().contains("not in the parent"), "got {err}");
    }

    #[tokio::test]
    async fn run_coordinator_requires_spawner() {
        let dir = TempDir::new().unwrap();
        let tool = RunCoordinatorTool::new();
        let res = tool.execute(plan_input(), ctx(&dir, None)).await;
        let err = res.expect_err("no spawner wired in this test ctx");
        assert!(
            err.to_string().contains("spawning is not available"),
            "got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // agent_to_agent_send (tools-p1p2 Phase B)
    // -----------------------------------------------------------------------

    /// Test messenger that records every delivery and can be configured to
    /// reject with a specific error.
    struct RecordingMessenger {
        calls: std::sync::Mutex<Vec<(String, String, String)>>,
        fail_with: Option<legion_runtime::MessengerError>,
    }

    impl RecordingMessenger {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(err: legion_runtime::MessengerError) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_with: Some(err),
            }
        }
    }

    #[async_trait]
    impl legion_runtime::AgentMessenger for RecordingMessenger {
        async fn send(
            &self,
            from_agent: &str,
            to_agent: &str,
            message: &str,
        ) -> Result<String, legion_runtime::MessengerError> {
            if let Some(err) = &self.fail_with {
                return Err(legion_runtime::MessengerError::Runtime(err.to_string()));
            }
            self.calls.lock().unwrap().push((
                from_agent.to_string(),
                to_agent.to_string(),
                message.to_string(),
            ));
            Ok(format!("delivered to {to_agent} (async)"))
        }
    }

    fn a2a_tool() -> AgentToAgentSendTool {
        AgentToAgentSendTool::new(Policy {
            approval: Approval::Prompt,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }

    fn ctx_with_messenger(
        dir: &TempDir,
        messenger: Arc<dyn legion_runtime::AgentMessenger>,
    ) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "main".to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: Some(messenger),
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
    async fn a2a_send_delivers_and_records_call() {
        let dir = TempDir::new().unwrap();
        let messenger = Arc::new(RecordingMessenger::new());
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "researcher", "message": "please review this"}),
                ctx_with_messenger(&dir, messenger.clone()),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        assert_eq!(res.content, "delivered to researcher (async)");
        let calls = messenger.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(
                "main".to_string(),
                "researcher".to_string(),
                "please review this".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a2a_send_requires_messenger() {
        let dir = TempDir::new().unwrap();
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "researcher", "message": "hi"}),
                ctx(&dir, None),
            )
            .await;
        let err = res.expect_err("no messenger wired in the default test ctx");
        assert!(
            err.to_string().contains("agent messenger not wired"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn a2a_send_rejects_self_send() {
        let dir = TempDir::new().unwrap();
        let messenger = Arc::new(RecordingMessenger::new());
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "main", "message": "note to self"}),
                ctx_with_messenger(&dir, messenger.clone()),
            )
            .await;
        let err = res.expect_err("send-to-self must be rejected");
        assert!(err.to_string().contains("cannot send to self"), "got {err}");
        assert!(messenger.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a2a_send_propagates_not_allowed() {
        let dir = TempDir::new().unwrap();
        let messenger = Arc::new(RecordingMessenger::failing(
            legion_runtime::MessengerError::NotAllowed {
                from: "main".to_string(),
                to: "researcher".to_string(),
            },
        ));
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "researcher", "message": "hi"}),
                ctx_with_messenger(&dir, messenger),
            )
            .await;
        let err = res.expect_err("NotAllowed must surface as a tool error");
        assert!(err.to_string().contains("not allowed"), "got {err}");
    }

    // -----------------------------------------------------------------------
    // swarm tools (multi-agent Phase D)
    // -----------------------------------------------------------------------

    fn prompt_policy() -> Policy {
        Policy {
            approval: Approval::Prompt,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    fn ctx_with_swarm(dir: &TempDir, swarm: Arc<legion_runtime::SwarmManager>) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "main".to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: None,
            swarm: Some(swarm),
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
            background_tasks: None,
            plan_mode_tracker: None,
        }
    }

    #[tokio::test]
    async fn swarm_spawn_requires_swarm() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSpawnTool::new(prompt_policy());
        let res = tool
            .execute(
                json!({"name": "worker", "prompt": "do things"}),
                ctx(&dir, None),
            )
            .await;
        let err = res.expect_err("no swarm wired in the default test ctx");
        assert!(
            err.to_string().contains("swarm is not available"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_send_requires_swarm() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSendTool::new(prompt_policy());
        let res = tool
            .execute(json!({"to": "worker", "message": "hi"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("no swarm wired in the default test ctx");
        assert!(
            err.to_string().contains("swarm is not available"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_status_requires_swarm() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmStatusTool::new(open_policy());
        let res = tool.execute(json!({}), ctx(&dir, None)).await;
        let err = res.expect_err("no swarm wired in the default test ctx");
        assert!(
            err.to_string().contains("swarm is not available"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_spawn_requires_name_and_prompt() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSpawnTool::new(prompt_policy());

        let res = tool
            .execute(json!({"prompt": "do things"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("missing name must fail");
        assert!(err.to_string().contains("'name'"), "got {err}");

        let res = tool
            .execute(json!({"name": "worker"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("missing prompt must fail");
        assert!(err.to_string().contains("'prompt'"), "got {err}");
    }

    #[tokio::test]
    async fn swarm_spawn_rejects_mcp_tools() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSpawnTool::new(prompt_policy());
        let res = tool
            .execute(
                json!({
                    "name": "worker",
                    "prompt": "do things",
                    "allowedTools": ["mcp__fs__read"]
                }),
                ctx(&dir, None),
            )
            .await;
        let err = res.expect_err("mcp tools must be rejected");
        assert!(
            err.to_string().contains("cannot be passed to a sub-agent"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_spawn_and_status_roundtrip() {
        let dir = TempDir::new().unwrap();
        let swarm = Arc::new(legion_runtime::SwarmManager::new(Arc::new(
            FakeSpawner::default(),
        )));
        let spawn = SwarmSpawnTool::new(prompt_policy());
        let res = spawn
            .execute(
                json!({"name": "worker", "prompt": "research this"}),
                ctx_with_swarm(&dir, swarm.clone()),
            )
            .await
            .unwrap();
        assert!(
            res.content.contains("teammate 'worker' spawned"),
            "got {}",
            res.content
        );

        // Wait for the first turn to complete, then check the status output.
        for _ in 0..200 {
            if swarm
                .status()
                .iter()
                .any(|t| t.name == "worker" && t.turns == 1)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let status = SwarmStatusTool::new(open_policy());
        let res = status
            .execute(json!({}), ctx_with_swarm(&dir, swarm))
            .await
            .unwrap();
        assert!(
            res.content.contains("worker [idle] turns=1 mailbox=0"),
            "got {}",
            res.content
        );
        assert!(
            res.content.contains("last: coordinated-output"),
            "got {}",
            res.content
        );
    }
}
