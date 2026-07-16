use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::approval::PermissionMode;
use crate::memory::MemoryBackend;
use crate::question::QuestionGate;
use crate::todo::SharedTodoStore;

pub use legion_core::config::ToolConfig;

/// Result of a background task once it has completed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackgroundTaskResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Snapshot of a background task's current output and state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackgroundTaskOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub is_running: bool,
}

/// Registry that tracks background tasks spawned by tools.
///
/// Implementations are expected to be thread-safe and shared across tool
/// invocations in the same session via [`ToolContext::background_tasks`].
#[async_trait]
pub trait BackgroundTaskRegistry: Send + Sync + std::fmt::Debug {
    /// Allocate a new unique task id.
    fn next_task_id(&self) -> String;

    /// Register a new background task.
    ///
    /// `handle` is the tokio task running the command; `log_path` is the file
    /// the task writes its stdout/stderr to. Returns the resolved task id
    /// (the registry may normalize or replace the requested id).
    async fn register(
        &self,
        task_id: String,
        handle: tokio::task::JoinHandle<Result<BackgroundTaskResult, String>>,
        log_path: PathBuf,
    ) -> Result<String, ToolError>;

    /// Wait for all given task ids to complete and return their results.
    async fn wait(
        &self,
        task_ids: &[String],
    ) -> Result<HashMap<String, BackgroundTaskResult>, ToolError>;

    /// Kill a running task.
    async fn kill(&self, task_id: &str) -> Result<(), ToolError>;

    /// Return the current output of a task without waiting for it to finish.
    async fn output(&self, task_id: &str) -> Result<BackgroundTaskOutput, ToolError>;
}

/// Context passed to a tool execution.
#[derive(Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub session_id: String,
    pub agent_id: String,
    /// Optional identifier of the sender that triggered the tool call.
    /// `None` is used when the call originates from the local runtime itself.
    pub sender: Option<String>,
    /// Optional memory backend for tools that need to search or retrieve
    /// persistent memory.
    pub memory: Option<Arc<dyn MemoryBackend>>,
    /// Optional sink for registering files that the tool has read. Used to
    /// redeclare viewed files after compaction.
    pub viewed_files: Option<Arc<std::sync::Mutex<HashSet<PathBuf>>>>,
    /// When `Some`, restricts this run to the named tool subset (permission
    /// narrowing for sub-agent runs). `None` means the full registry.
    pub allowed_tools: Option<Vec<String>>,
    /// Optional handle for delegating to sub-agents. Present only when the
    /// gateway wired a `RuntimeSubagentSpawner` into the runtime.
    pub spawner: Option<Arc<dyn crate::subagent::SubagentSpawner>>,
    /// Optional handle for fire-and-forget agent-to-agent messaging
    /// (tools-p1p2 Phase B). Present only when the gateway wired a
    /// `RuntimeAgentMessenger` into the runtime.
    pub messenger: Option<Arc<dyn crate::messenger::AgentMessenger>>,
    /// Optional handle for the in-process swarm of named teammates
    /// (multi-agent Phase D). Present only when the gateway wired a
    /// `SwarmManager` into the runtime.
    pub swarm: Option<Arc<crate::swarm::SwarmManager>>,
    /// Nesting depth of the current run (0 for top-level). Forwarded to
    /// `spawn_subagent` so the spawner can enforce `subagents.max_depth`.
    pub depth: u8,
    /// Snapshot of the current run's conversation, taken at the start of the
    /// tool batch. `spawn_subagent` uses it to seed a Fork child's history.
    pub parent_history: Option<Arc<Vec<legion_provider::types::ChatMessage>>>,
    /// Optional question gate for interactive `ask_user` prompts.
    pub question_gate: Option<Arc<QuestionGate>>,
    /// Optional todo store for the `todo_write` tool to update the session
    /// checklist. When `None` the tool reports that todos are unavailable.
    pub todo_store: Option<SharedTodoStore>,
    /// Optional registry for background tasks spawned by the `exec` tool and
    /// managed by `wait_tasks`, `kill_task`, and `get_task_output`.
    pub background_tasks: Option<Arc<dyn BackgroundTaskRegistry>>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("workspace", &self.workspace)
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("sender", &self.sender)
            .field("memory", &self.memory.is_some())
            .field("viewed_files", &self.viewed_files.is_some())
            .field("allowed_tools", &self.allowed_tools)
            .field("spawner", &self.spawner.is_some())
            .field("messenger", &self.messenger.is_some())
            .field("swarm", &self.swarm.is_some())
            .field("depth", &self.depth)
            .field(
                "parent_history",
                &self.parent_history.as_ref().map(|h| h.len()),
            )
            .field("question_gate", &self.question_gate.is_some())
            .field("todo_store", &self.todo_store.is_some())
            .field("background_tasks", &self.background_tasks.is_some())
            .finish()
    }
}

/// A tool call emitted by the assistant.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl From<legion_provider::types::ToolCall> for ToolCall {
    fn from(tc: legion_provider::types::ToolCall) -> Self {
        Self {
            id: tc.id,
            name: tc.function.name,
            arguments: tc.function.arguments,
        }
    }
}

impl From<&legion_provider::types::ToolCall> for ToolCall {
    fn from(tc: &legion_provider::types::ToolCall) -> Self {
        Self {
            id: tc.id.clone(),
            name: tc.function.name.clone(),
            arguments: tc.function.arguments.clone(),
        }
    }
}

/// The result of executing a tool.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Errors that can occur while executing a tool.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool execution failed: {0}")]
    Execution(String),

    #[error("invalid tool parameters: {0}")]
    InvalidParams(String),

    #[error("tool '{0}' not found")]
    NotFound(String),

    #[error("tool '{0}' is not allowed in this context")]
    NotAllowed(String),
}

impl From<serde_json::Error> for ToolError {
    fn from(err: serde_json::Error) -> Self {
        ToolError::InvalidParams(err.to_string())
    }
}

/// Approval level for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// No approval required.
    Off,
    /// Approval is requested; in interactive sessions this triggers a prompt,
    /// in unattended sessions it fails closed.
    Prompt,
    /// Approval is required unless the sender is present in `allow_from`.
    Required,
}

impl FromStr for Approval {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Approval::Off),
            "prompt" => Ok(Approval::Prompt),
            "required" => Ok(Approval::Required),
            _ => Err(format!("unknown approval level: {s}")),
        }
    }
}

impl From<Approval> for PermissionMode {
    /// Behavior-preserving mapping from the legacy approval enum.
    ///
    /// `Approval::Required` maps to `PermissionMode::Default` so that existing
    /// interactive prompts are preserved; unattended runs continue to fail
    /// closed at the approval gate.
    fn from(approval: Approval) -> Self {
        match approval {
            Approval::Off => PermissionMode::BypassPermissions,
            Approval::Prompt | Approval::Required => PermissionMode::Default,
        }
    }
}

/// Per-tool policy derived from configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub approval: Approval,
    /// Optional richer permission mode. When `None`, the effective mode is
    /// derived from [`Policy::approval`].
    pub permission_mode: Option<PermissionMode>,
    pub allow_from: Vec<String>,
    pub workspace_only: bool,
}

impl Policy {
    /// Build a policy from the raw config for a specific tool.
    ///
    /// `default_approval` is used when no explicit `approval` key is present.
    /// If `permissionMode` is present it takes precedence; otherwise the mode
    /// is derived from `approval`.
    pub fn from_config(config: Option<&ToolConfig>, default_approval: Approval) -> Self {
        let config = config.cloned().unwrap_or_default();
        let approval = config
            .approval
            .as_deref()
            .and_then(|s| s.parse::<Approval>().ok())
            .unwrap_or(default_approval);
        let permission_mode = config
            .permission_mode
            .as_deref()
            .and_then(|s| s.parse::<PermissionMode>().ok());

        Self {
            approval,
            permission_mode,
            allow_from: config.allow_from,
            workspace_only: config.workspace_only.unwrap_or(false),
        }
    }

    /// The effective permission mode for this policy.
    ///
    /// When a `permission_mode` is configured explicitly it wins. Otherwise the
    /// legacy `approval` value is converted with the behavior-preserving
    /// mapping: `Off` -> `BypassPermissions`, `Prompt`/`Required` -> `Default`.
    pub fn effective_permission_mode(&self) -> PermissionMode {
        self.permission_mode.unwrap_or(self.approval.into())
    }

    /// Returns `true` if the sender is explicitly allowed.
    pub fn sender_allowed(&self, sender: Option<&str>) -> bool {
        match sender {
            Some(sender) => self
                .allow_from
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(sender)),
            // Calls originating from the runtime itself (no sender) are treated as
            // local and therefore allowed when `local` is in the allow-list.
            None => self
                .allow_from
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case("local")),
        }
    }
}

/// Check whether the tool may execute under the given policy.
pub fn check_policy(policy: &Policy, sender: Option<&str>) -> Result<(), String> {
    if policy.sender_allowed(sender) {
        return Ok(());
    }

    match policy.approval {
        Approval::Off => Ok(()),
        Approval::Prompt | Approval::Required => Err(format!(
            "tool requires approval; sender '{sender:?}' is not in allow_from list",
        )),
    }
}

/// Apply a [`PermissionMode`] to a permission decision returned by the policy
/// decider. Hard denials are never overridden; `Prompt` may be promoted to
/// `Allow` or demoted to `Deny` depending on the mode and the tool.
pub fn apply_permission_mode(
    mode: PermissionMode,
    permission: Permission,
    tool: &dyn Tool,
    input: &serde_json::Value,
) -> Permission {
    match permission {
        Permission::Allow | Permission::Deny { .. } => permission,
        Permission::Prompt { message } => match mode {
            PermissionMode::Default => Permission::Prompt { message },
            PermissionMode::BypassPermissions => Permission::Allow,
            PermissionMode::DontAsk => Permission::Deny {
                reason: format!(
                    "tool '{}' requires approval but permission mode is dontAsk",
                    tool.name()
                ),
            },
            PermissionMode::AcceptEdits => {
                if matches!(tool.name(), "write" | "edit" | "apply_patch") {
                    Permission::Allow
                } else {
                    Permission::Prompt { message }
                }
            }
            PermissionMode::Auto | PermissionMode::Plan => {
                if tool.is_read_only(input) {
                    Permission::Allow
                } else {
                    Permission::Prompt { message }
                }
            }
        },
    }
}

/// Build a decider that uses each tool's configured [`Policy`] to decide whether
/// execution is allowed, should prompt for approval, or is denied.
///
/// When a tool's policy specifies a [`PermissionMode`], the mode is resolved
/// here so that `execute_tool_call` only needs to handle the session-level
/// override.
pub fn build_policy_decider(registry: Arc<dyn ToolRegistry>) -> CanUseToolFn {
    Arc::new(move |name, input, sender| {
        let registry = registry.clone();
        let name = name.to_string();
        let sender = sender.map(|s| s.to_string());
        let input = input.clone();
        Box::pin(async move {
            match registry.get(&name) {
                Some(tool) => {
                    let policy = tool.policy();
                    match check_policy(policy, sender.as_deref()) {
                        Ok(()) => Permission::Allow,
                        Err(_) => {
                            let mode = policy.effective_permission_mode();
                            apply_permission_mode(
                                mode,
                                Permission::Prompt {
                                    message: format!("tool '{name}' requires approval"),
                                },
                                tool.as_ref(),
                                &input,
                            )
                        }
                    }
                }
                None => Permission::Deny {
                    reason: format!("tool '{name}' not found"),
                },
            }
        })
    })
}

/// Permission decision produced by a pre-tool hook or policy check.
#[derive(Debug, Clone, PartialEq)]
pub enum Permission {
    /// The tool may execute with the provided input.
    Allow,
    /// The tool needs human approval before executing. Without an approval gate
    /// wired into the pipeline this is treated as a denial (fail-closed).
    Prompt { message: String },
    /// The tool is not allowed to execute.
    Deny { reason: String },
}

impl Permission {
    /// Returns `true` if this decision allows execution.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Permission::Allow)
    }
}

/// A function that decides whether a tool may execute in the current context.
///
/// The decider is asynchronous so it can await human approval (via an
/// [`ApprovalGate`](crate::approval::ApprovalGate)) when it returns
/// [`Permission::Prompt`]. Arguments: tool name, parsed input object, optional
/// sender identifier.
pub type CanUseToolFn = Arc<
    dyn Fn(
            &str,
            &serde_json::Value,
            Option<&str>,
        ) -> Pin<Box<dyn Future<Output = Permission> + Send + 'static>>
        + Send
        + Sync,
>;

/// A callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;

    /// Returns the approval policy for this tool.
    fn policy(&self) -> &Policy;

    /// Returns `true` if this tool only reads state and never mutates it.
    ///
    /// Default is `false` (fail-closed): new tools are assumed to be mutating
    /// until they explicitly opt-in to being read-only.
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    /// Returns `true` if this tool is destructive (e.g. deletes data).
    ///
    /// Default is `false`. Destructive tools are always scheduled serially and
    /// may trigger additional confirmation logic.
    fn is_destructive(&self, _input: &serde_json::Value) -> bool {
        false
    }

    /// Returns `true` if this specific invocation can safely run concurrently
    /// with other concurrent-safe tool calls.
    ///
    /// Default is `false`: new tools are assumed unsafe for concurrency until
    /// proven otherwise.
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    /// Additional semantic validation beyond JSON Schema.
    ///
    /// This is called after schema validation succeeds. Implementations can
    /// check semantic constraints (e.g. old_string must differ from new_string,
    /// a target path must exist, etc.).
    fn validate_input(&self, _input: &serde_json::Value) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError>;
}

/// A registry that looks up tools by name and exposes provider-facing definitions.
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;

    fn definitions(&self) -> Vec<legion_provider::types::ToolDefinition>;
}

/// Convenience extension trait for converting a `Tool` into a provider definition.
pub trait ToolDefinitionExt {
    fn definition(&self) -> legion_provider::types::ToolDefinition;
}

impl<T: Tool + ?Sized> ToolDefinitionExt for T {
    fn definition(&self) -> legion_provider::types::ToolDefinition {
        legion_provider::types::ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.schema(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_off_allows_anyone() {
        let policy = Policy {
            approval: Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        };
        assert!(check_policy(&policy, Some("unknown")).is_ok());
        assert!(check_policy(&policy, None).is_ok());
    }

    #[test]
    fn required_blocks_unknown_sender() {
        let policy = Policy {
            approval: Approval::Required,
            permission_mode: None,
            allow_from: vec!["local".to_string()],
            workspace_only: false,
        };
        assert!(check_policy(&policy, Some("tg:123")).is_err());
        assert!(check_policy(&policy, None).is_ok());
    }

    #[test]
    fn required_allows_matching_sender() {
        let policy = Policy {
            approval: Approval::Required,
            permission_mode: None,
            allow_from: vec!["tg:123".to_string()],
            workspace_only: false,
        };
        assert!(check_policy(&policy, Some("tg:123")).is_ok());
    }

    #[test]
    fn from_config_defaults() {
        let cfg = ToolConfig::default();
        let policy = Policy::from_config(Some(&cfg), Approval::Off);
        assert_eq!(policy.approval, Approval::Off);
        assert!(policy.allow_from.is_empty());
        assert!(!policy.workspace_only);
    }

    #[test]
    fn from_config_parses_required() {
        let cfg = ToolConfig {
            approval: Some("required".to_string()),
            allow_from: vec!["local".to_string()],
            ..Default::default()
        };
        let policy = Policy::from_config(Some(&cfg), Approval::Off);
        assert_eq!(policy.approval, Approval::Required);
        assert_eq!(policy.allow_from, vec!["local".to_string()]);
    }

    #[test]
    fn from_config_invalid_approval_falls_back_to_default() {
        let cfg = ToolConfig {
            approval: Some("sometimes".to_string()),
            ..Default::default()
        };
        let policy = Policy::from_config(Some(&cfg), Approval::Prompt);
        assert_eq!(policy.approval, Approval::Prompt);
    }

    #[test]
    fn from_config_parses_permission_mode() {
        let cfg = ToolConfig {
            permission_mode: Some("auto".to_string()),
            ..Default::default()
        };
        let policy = Policy::from_config(Some(&cfg), Approval::Prompt);
        assert_eq!(policy.permission_mode, Some(PermissionMode::Auto));
        assert_eq!(policy.effective_permission_mode(), PermissionMode::Auto);
    }

    #[test]
    fn from_config_permission_mode_takes_precedence_over_approval() {
        let cfg = ToolConfig {
            approval: Some("required".to_string()),
            permission_mode: Some("bypass_permissions".to_string()),
            ..Default::default()
        };
        let policy = Policy::from_config(Some(&cfg), Approval::Prompt);
        assert_eq!(policy.approval, Approval::Required);
        assert_eq!(
            policy.effective_permission_mode(),
            PermissionMode::BypassPermissions
        );
    }

    #[test]
    fn sender_allowed_is_case_insensitive() {
        let policy = Policy {
            approval: Approval::Required,
            permission_mode: None,
            allow_from: vec!["alice".to_string()],
            workspace_only: false,
        };
        assert!(policy.sender_allowed(Some("Alice")));
        assert!(policy.sender_allowed(Some("ALICE")));
        assert!(!policy.sender_allowed(Some("bob")));
    }

    #[test]
    fn check_policy_treats_prompt_like_required() {
        let prompt = Policy {
            approval: Approval::Prompt,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        };
        let required = Policy {
            approval: Approval::Required,
            permission_mode: None,
            ..prompt.clone()
        };
        // Unknown senders are rejected identically, whether attended or not
        // (the interactive/unattended split happens later at the gate).
        assert_eq!(
            check_policy(&prompt, Some("unknown")),
            check_policy(&required, Some("unknown"))
        );
        assert!(check_policy(&prompt, Some("unknown")).is_err());
        // Calls with no sender (local runtime) are treated the same way.
        assert_eq!(check_policy(&prompt, None), check_policy(&required, None));
        assert!(check_policy(&prompt, None).is_err());
    }

    use async_trait::async_trait;
    use std::collections::HashMap;

    struct FakeTool {
        tool_name: &'static str,
        policy: Policy,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.tool_name
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn policy(&self) -> &Policy {
            &self.policy
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok(""))
        }
    }

    struct FakeRegistry {
        tools: HashMap<String, Arc<dyn Tool>>,
    }

    #[async_trait]
    impl ToolRegistry for FakeRegistry {
        fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
            self.tools.get(name).cloned()
        }
        fn definitions(&self) -> Vec<legion_provider::types::ToolDefinition> {
            self.tools.values().map(|t| t.definition()).collect()
        }
    }

    fn registry_with_policy(approval: Approval, allow_from: Vec<String>) -> Arc<dyn ToolRegistry> {
        let mut tools = HashMap::new();
        tools.insert(
            "fake".to_string(),
            Arc::new(FakeTool {
                tool_name: "fake",
                policy: Policy {
                    approval,
                    permission_mode: None,
                    allow_from,
                    workspace_only: false,
                },
            }) as Arc<dyn Tool>,
        );
        Arc::new(FakeRegistry { tools })
    }

    #[tokio::test]
    async fn policy_decider_allows_off_tool() {
        let registry = registry_with_policy(Approval::Off, vec![]);
        let decider = build_policy_decider(registry);
        let decision = decider("fake", &serde_json::json!({}), Some("unknown")).await;
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn policy_decider_prompts_required_tool() {
        let registry = registry_with_policy(Approval::Required, vec![]);
        let decider = build_policy_decider(registry);
        let decision = decider("fake", &serde_json::json!({}), Some("unknown")).await;
        assert!(matches!(decision, Permission::Prompt { .. }));
    }

    #[tokio::test]
    async fn policy_decider_prompts_prompt_tool() {
        let registry = registry_with_policy(Approval::Prompt, vec![]);
        let decider = build_policy_decider(registry);
        let decision = decider("fake", &serde_json::json!({}), Some("unknown")).await;
        assert!(matches!(decision, Permission::Prompt { .. }));
    }

    #[tokio::test]
    async fn policy_decider_allows_allowed_sender() {
        let registry = registry_with_policy(Approval::Required, vec!["tg:123".to_string()]);
        let decider = build_policy_decider(registry);
        let decision = decider("fake", &serde_json::json!({}), Some("tg:123")).await;
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn policy_decider_denies_unknown_tool() {
        let registry = registry_with_policy(Approval::Off, vec![]);
        let decider = build_policy_decider(registry);
        let decision = decider("missing", &serde_json::json!({}), None).await;
        assert!(matches!(decision, Permission::Deny { .. }));
    }

    fn read_only_policy() -> &'static Policy {
        use std::sync::OnceLock;
        static POLICY: OnceLock<Policy> = OnceLock::new();
        POLICY.get_or_init(|| Policy {
            approval: Approval::Prompt,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }

    struct ReadOnlyFakeTool;

    #[async_trait]
    impl Tool for ReadOnlyFakeTool {
        fn name(&self) -> &str {
            "read_only_fake"
        }
        fn description(&self) -> &str {
            "read only fake"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn policy(&self) -> &Policy {
            read_only_policy()
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok(""))
        }
    }

    struct MutatingFakeTool;

    #[async_trait]
    impl Tool for MutatingFakeTool {
        fn name(&self) -> &str {
            "write"
        }
        fn description(&self) -> &str {
            "write"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn policy(&self) -> &Policy {
            read_only_policy()
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            false
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok(""))
        }
    }

    #[test]
    fn apply_mode_default_keeps_prompt() {
        let tool = ReadOnlyFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::Default,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(matches!(result, Permission::Prompt { .. }));
    }

    #[test]
    fn apply_mode_bypass_allows_prompt() {
        let tool = ReadOnlyFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::BypassPermissions,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(result.is_allowed());
    }

    #[test]
    fn apply_mode_bypass_respects_deny() {
        let tool = ReadOnlyFakeTool;
        let permission = Permission::Deny {
            reason: "no".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::BypassPermissions,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(matches!(result, Permission::Deny { .. }));
    }

    #[test]
    fn apply_mode_dont_ask_denies_prompt() {
        let tool = ReadOnlyFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::DontAsk,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(matches!(result, Permission::Deny { .. }));
    }

    #[test]
    fn apply_mode_auto_allows_read_only() {
        let tool = ReadOnlyFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::Auto,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(result.is_allowed());
    }

    #[test]
    fn apply_mode_auto_keeps_prompt_for_mutating() {
        let tool = MutatingFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::Auto,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(matches!(result, Permission::Prompt { .. }));
    }

    #[test]
    fn apply_mode_accept_edits_allows_write_tools() {
        let tool = MutatingFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::AcceptEdits,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(result.is_allowed());
    }

    #[test]
    fn apply_mode_accept_edits_keeps_prompt_for_non_edit() {
        let tool = ReadOnlyFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::AcceptEdits,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(matches!(result, Permission::Prompt { .. }));
    }

    #[test]
    fn apply_mode_plan_allows_read_only() {
        let tool = ReadOnlyFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::Plan,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(result.is_allowed());
    }

    #[test]
    fn apply_mode_plan_keeps_prompt_for_mutating() {
        let tool = MutatingFakeTool;
        let permission = Permission::Prompt {
            message: "ask".to_string(),
        };
        let result = apply_permission_mode(
            PermissionMode::Plan,
            permission,
            &tool,
            &serde_json::json!({}),
        );
        assert!(matches!(result, Permission::Prompt { .. }));
    }

    #[test]
    fn approval_to_permission_mode_preserves_existing_behavior() {
        assert_eq!(
            PermissionMode::from(Approval::Off),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            PermissionMode::from(Approval::Prompt),
            PermissionMode::Default
        );
        // Required maps to Default so that interactive prompts and unattended
        // gate denials are preserved.
        assert_eq!(
            PermissionMode::from(Approval::Required),
            PermissionMode::Default
        );
    }
}
