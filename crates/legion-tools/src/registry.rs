use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use legion_core::config::Config;
use legion_mcp::McpToolAdapter;
use legion_provider::types::ToolDefinition;
use legion_runtime::tools::ToolDefinitionExt;
use legion_runtime::{Tool, ToolRegistry};

use crate::mcp::McpTool;

use crate::ask_user::AskUserTool;
use crate::browser::BrowserTool;
use crate::grep::GrepTool;
use crate::list_dir::ListDirTool;
use crate::policy::{Approval, Policy};
use crate::sandbox::{
    CubeSandboxBackend, ExecResult, LocalSandboxBackend, RestrictedConfig,
    RestrictedSandboxBackend, SandboxBackend, SandboxBackendConfig, SandboxCapabilities,
    SandboxError, SandboxMode, SandboxUnavailableReason, sandbox_available,
};
use crate::scheduler::{SchedulerCreateTool, SchedulerDeleteTool, SchedulerListTool};
use crate::todo::TodoWriteTool;
use crate::tools::{
    AgentToAgentSendTool, ApplyPatchTool, EditTool, ExecTool, MemoryGetTool, MemoryIndexTool,
    MemorySearchTool, ReadTool, RunCoordinatorTool, SpawnSubagentTool, SwarmSendTool,
    SwarmSpawnTool, SwarmStatusTool, WebFetchTool, WebSearchTool, WriteTool,
};
use std::path::Path;

fn build_exec_tool(
    exec_config: &HashMap<String, legion_core::config::ToolConfig>,
    policy: Policy,
) -> ExecTool {
    let cfg = exec_config.get("exec");
    let mode = cfg
        .and_then(|c| c.sandbox.as_ref())
        .and_then(|s| s.parse::<SandboxMode>().ok())
        .unwrap_or(SandboxMode::Off);

    match mode {
        SandboxMode::Off => ExecTool::with_backend(policy, Arc::new(LocalSandboxBackend::new())),
        SandboxMode::Restricted => {
            let restricted_config: RestrictedConfig = cfg
                .and_then(|c| c.extra.get("restrictedConfig"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            match sandbox_available(SandboxMode::Restricted) {
                Ok(()) => ExecTool::with_backend(
                    policy,
                    Arc::new(RestrictedSandboxBackend::new(restricted_config)),
                ),
                Err(reason) => {
                    tracing::warn!(
                        error = %reason,
                        "restricted sandbox is unavailable; exec will fail at runtime"
                    );
                    ExecTool::with_backend(policy, Arc::new(UnavailableBackend::new(reason)))
                }
            }
        }
        SandboxMode::Cube => {
            let backend_config: SandboxBackendConfig = cfg
                .and_then(|c| c.extra.get("sandboxConfig"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let backend = CubeSandboxBackend::new(backend_config)
                .map_err(|e| tracing::warn!("failed to create CubeSandbox backend: {}", e))
                .ok();
            match backend {
                Some(b) => ExecTool::with_backend(policy, Arc::new(b)),
                None => ExecTool::with_backend(policy, Arc::new(LocalSandboxBackend::new())),
            }
        }
    }
}

/// Backend placeholder used when a configured sandbox profile is unavailable.
#[derive(Debug)]
struct UnavailableBackend {
    reason: SandboxUnavailableReason,
}

impl UnavailableBackend {
    fn new(reason: SandboxUnavailableReason) -> Self {
        Self { reason }
    }
}

#[async_trait]
impl SandboxBackend for UnavailableBackend {
    async fn exec(
        &self,
        _command: &str,
        _cwd: &Path,
        _timeout_secs: u64,
    ) -> Result<ExecResult, SandboxError> {
        Err(SandboxError::RequestFailed(self.reason.to_string()))
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::default()
    }
}

/// Built-in registry that exposes all core Legion tools.
pub struct CoreToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl CoreToolRegistry {
    /// Construct the registry from the global configuration without MCP tools.
    pub fn new(config: &Config) -> Self {
        Self::new_with_mcp(config, None)
    }

    /// Construct the registry from the global configuration, optionally merging
    /// in tools surfaced by an MCP manager. Built-in tools take precedence over
    /// MCP tools with the same name.
    pub fn new_with_mcp(config: &Config, mcp_tools: Option<&[McpToolAdapter]>) -> Self {
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

        let read_policy = Policy::from_config(config.tools.get("read"), Approval::Off);
        tools.insert("read".to_string(), Arc::new(ReadTool::new(read_policy)));

        let list_dir_policy = Policy::from_config(config.tools.get("list_dir"), Approval::Off);
        tools.insert(
            "list_dir".to_string(),
            Arc::new(ListDirTool::new(list_dir_policy)),
        );

        let grep_policy = Policy::from_config(config.tools.get("grep"), Approval::Off);
        tools.insert("grep".to_string(), Arc::new(GrepTool::new(grep_policy)));

        tools.insert("ask_user".to_string(), Arc::new(AskUserTool::new()));

        let write_policy = Policy::from_config(config.tools.get("write"), Approval::Off);
        tools.insert("write".to_string(), Arc::new(WriteTool::new(write_policy)));

        let edit_policy = Policy::from_config(config.tools.get("edit"), Approval::Off);
        tools.insert("edit".to_string(), Arc::new(EditTool::new(edit_policy)));

        let patch_policy = Policy::from_config(config.tools.get("apply_patch"), Approval::Off);
        tools.insert(
            "apply_patch".to_string(),
            Arc::new(ApplyPatchTool::new(patch_policy)),
        );

        // Exec defaults to required approval.
        let exec_policy = Policy::from_config(config.tools.get("exec"), Approval::Required);
        let exec_tool = build_exec_tool(&config.tools, exec_policy);
        tools.insert("exec".to_string(), Arc::new(exec_tool));

        let web_fetch_policy = Policy::from_config(config.tools.get("web_fetch"), Approval::Off);
        tools.insert(
            "web_fetch".to_string(),
            Arc::new(WebFetchTool::new(web_fetch_policy)),
        );

        let web_search_policy = Policy::from_config(config.tools.get("web_search"), Approval::Off);
        tools.insert(
            "web_search".to_string(),
            Arc::new(WebSearchTool::new(web_search_policy)),
        );

        tools.insert(
            "memory_search".to_string(),
            Arc::new(MemorySearchTool::new()),
        );
        tools.insert("memory_get".to_string(), Arc::new(MemoryGetTool::new()));
        tools.insert("memory_index".to_string(), Arc::new(MemoryIndexTool::new()));
        tools.insert(
            "spawn_subagent".to_string(),
            Arc::new(SpawnSubagentTool::new()),
        );
        tools.insert(
            "run_coordinator".to_string(),
            Arc::new(RunCoordinatorTool::new()),
        );

        // Cross-agent messaging defaults to Prompt: the originating user
        // confirms before a message is delivered to another agent. The
        // target-side allowFrom check is enforced by the messenger.
        let a2a_policy =
            Policy::from_config(config.tools.get("agent_to_agent_send"), Approval::Prompt);
        tools.insert(
            "agent_to_agent_send".to_string(),
            Arc::new(AgentToAgentSendTool::new(a2a_policy)),
        );

        // Swarm teammates (multi-agent Phase D). spawn/send default to
        // Prompt: spawning derives a paid background agent and sending
        // triggers a new turn on one; status is read-only.
        let swarm_spawn_policy =
            Policy::from_config(config.tools.get("swarm_spawn"), Approval::Prompt);
        tools.insert(
            "swarm_spawn".to_string(),
            Arc::new(SwarmSpawnTool::new(swarm_spawn_policy)),
        );
        let swarm_send_policy =
            Policy::from_config(config.tools.get("swarm_send"), Approval::Prompt);
        tools.insert(
            "swarm_send".to_string(),
            Arc::new(SwarmSendTool::new(swarm_send_policy)),
        );
        let swarm_status_policy =
            Policy::from_config(config.tools.get("swarm_status"), Approval::Off);
        tools.insert(
            "swarm_status".to_string(),
            Arc::new(SwarmStatusTool::new(swarm_status_policy)),
        );

        // Scheduler tools (automation Phase A). Creating and deleting jobs
        // mutate persisted automation state, so they default to Prompt; listing
        // is read-only and defaults to Off.
        tools.insert(
            "scheduler_create".to_string(),
            Arc::new(SchedulerCreateTool::new()),
        );
        tools.insert(
            "scheduler_delete".to_string(),
            Arc::new(SchedulerDeleteTool::new()),
        );
        tools.insert(
            "scheduler_list".to_string(),
            Arc::new(SchedulerListTool::new()),
        );

        // browser (tools-p1p2 Phase C). Defaults to Approval::Required: it
        // drives a real browser over the network (gap doc §4.4/§6.2). The
        // backend config rides in the opaque ToolConfig.extra: `cdpUrl`
        // (WebSocket URL of a Chromium DevTools endpoint) and
        // `timeoutSeconds` (default 30).
        let browser_cfg = config.tools.get("browser");
        let browser_policy = Policy::from_config(browser_cfg, Approval::Required);
        let browser_cdp_url = browser_cfg
            .and_then(|c| c.extra.get("cdpUrl"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let browser_timeout = browser_cfg
            .and_then(|c| c.extra.get("timeoutSeconds"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(30);
        tools.insert(
            "browser".to_string(),
            Arc::new(BrowserTool::new(
                browser_policy,
                browser_cdp_url,
                std::time::Duration::from_secs(browser_timeout),
            )),
        );

        // Session todo list. Off by default approval-wise; it only mutates the
        // agent's own checklist, not the workspace.
        if config.todos.enabled {
            tools.insert(
                "todo_write".to_string(),
                Arc::new(TodoWriteTool::new(Policy {
                    approval: Approval::Off,
                    permission_mode: None,
                    allow_from: vec![],
                    workspace_only: false,
                })),
            );
        }

        if let Some(mcp_tools) = mcp_tools {
            for adapter in mcp_tools {
                let name = adapter.qualified_name().to_string();
                if tools.contains_key(&name) {
                    tracing::warn!(
                        tool = %name,
                        "MCP tool conflicts with a built-in tool; built-in wins"
                    );
                    continue;
                }
                tools.insert(name, Arc::new(McpTool::new(adapter.clone())));
            }
        }

        Self { tools }
    }

    /// Register an additional tool after construction.
    ///
    /// A tool whose name is already registered is **not** overwritten; a
    /// warning is logged instead (same conflict handling as MCP tools).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            tracing::warn!(tool = %name, "tool already registered; keeping existing tool");
            return;
        }
        self.tools.insert(name, tool);
    }

    /// Return the names of all registered tools.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}

#[async_trait]
impl ToolRegistry for CoreToolRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_core::config::Config;

    #[test]
    fn registry_contains_core_tools() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let registry = CoreToolRegistry::new(&config);

        for name in [
            "read",
            "list_dir",
            "grep",
            "write",
            "edit",
            "apply_patch",
            "exec",
            "web_fetch",
            "web_search",
            "memory_search",
            "memory_get",
            "memory_index",
            "scheduler_create",
            "scheduler_delete",
            "scheduler_list",
        ] {
            assert!(registry.get(name).is_some(), "missing tool {}", name);
        }
    }

    #[test]
    fn definitions_match_registry() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let registry = CoreToolRegistry::new(&config);
        let defs = registry.definitions();
        assert_eq!(defs.len(), registry.names().len());
        for def in &defs {
            assert!(registry.get(&def.name).is_some());
        }
    }

    #[test]
    fn registry_parses_cube_sandbox_config() {
        let config = Config::from_json(
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "tools": {
                    "exec": {
                        "sandbox": "cube",
                        "sandboxConfig": {
                            "templateId": "tpl-test",
                            "apiUrl": "http://127.0.0.1:3000",
                            "apiKey": "cube-key"
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let registry = CoreToolRegistry::new(&config);
        assert!(registry.get("exec").is_some());
    }

    fn test_ctx(workspace: &Path) -> legion_runtime::ToolContext {
        legion_runtime::ToolContext {
            workspace: workspace.to_path_buf(),
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
        }
    }

    #[tokio::test]
    async fn exec_tool_with_restricted_sandbox_is_registered() {
        let config = Config::from_json(
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "tools": { "exec": { "sandbox": "restricted" } }
            }"#,
        )
        .unwrap();
        let registry = CoreToolRegistry::new(&config);
        let tool = registry.get("exec").expect("exec tool registered");

        let tmp = tempfile::TempDir::new().unwrap();
        let res = tool
            .execute(
                serde_json::json!({"command": "echo pwned > /etc/passwd"}),
                test_ctx(tmp.path()),
            )
            .await;

        match sandbox_available(SandboxMode::Restricted) {
            Ok(()) => {
                // The restricted backend runs pre_exec_guard first, so a write
                // to a sensitive path is rejected before the command runs.
                let err = res.expect_err("restricted backend must guard writes to sensitive paths");
                assert!(
                    err.to_string().contains("sensitive path"),
                    "unexpected error: {err}"
                );
            }
            Err(reason) => {
                // Fail closed: an unavailable restricted profile must not fall
                // back to the unsandboxed local backend.
                let err = res.expect_err("unavailable restricted sandbox must fail closed");
                assert!(
                    err.to_string().contains(&reason.to_string()),
                    "unexpected error: {err}"
                );
            }
        }
    }

    #[tokio::test]
    async fn exec_tool_with_invalid_cube_config_falls_back_to_local() {
        // sandboxConfig without a templateId fails CubeSandboxBackend::new;
        // the registry currently falls back to the local backend.
        let config = Config::from_json(
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "tools": {
                    "exec": {
                        "sandbox": "cube",
                        "sandboxConfig": { "apiUrl": "http://127.0.0.1:3000" }
                    }
                }
            }"#,
        )
        .unwrap();
        let registry = CoreToolRegistry::new(&config);
        let tool = registry.get("exec").expect("exec tool registered");

        let tmp = tempfile::TempDir::new().unwrap();
        let res = tool
            .execute(
                serde_json::json!({"command": "echo hi"}),
                test_ctx(tmp.path()),
            )
            .await
            .expect("local fallback executes the command");

        let parsed: serde_json::Value = serde_json::from_str(&res.content).unwrap();
        assert_eq!(parsed["exit_code"].as_i64(), Some(0));
        assert!(parsed["stdout"].as_str().unwrap().contains("hi"));
    }

    #[test]
    fn registry_parses_browser_backend_config() {
        let config = Config::from_json(
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "tools": {
                    "browser": {
                        "approval": "off",
                        "cdpUrl": "ws://127.0.0.1:9222/devtools/browser/abc",
                        "timeoutSeconds": 5
                    }
                }
            }"#,
        )
        .unwrap();
        let registry = CoreToolRegistry::new(&config);
        let tool = registry.get("browser").expect("browser tool registered");
        assert_eq!(tool.policy().approval, Approval::Off);
    }

    #[test]
    fn browser_tool_registered_with_defaults_when_unconfigured() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let registry = CoreToolRegistry::new(&config);
        let tool = registry.get("browser").expect("browser tool registered");
        assert_eq!(tool.policy().approval, Approval::Required);
    }

    #[test]
    fn swarm_tools_registered_with_default_policies() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let registry = CoreToolRegistry::new(&config);
        assert_eq!(
            registry
                .get("swarm_spawn")
                .expect("swarm_spawn registered")
                .policy()
                .approval,
            Approval::Prompt
        );
        assert_eq!(
            registry
                .get("swarm_send")
                .expect("swarm_send registered")
                .policy()
                .approval,
            Approval::Prompt
        );
        assert_eq!(
            registry
                .get("swarm_status")
                .expect("swarm_status registered")
                .policy()
                .approval,
            Approval::Off
        );
    }

    fn dummy_adapter(server: &str, tool: &str) -> McpToolAdapter {
        use async_trait::async_trait;
        use legion_mcp::client::{McpClient, McpError, McpToolDesc, McpToolResult};
        use serde_json::Value;

        struct Dummy;
        #[async_trait]
        impl McpClient for Dummy {
            fn server_name(&self) -> &str {
                "dummy"
            }
            async fn connect(&self) -> Result<(), McpError> {
                Ok(())
            }
            async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
                Ok(Vec::new())
            }
            async fn call_tool(&self, _: &str, _: Value) -> Result<McpToolResult, McpError> {
                Ok(McpToolResult {
                    content: Value::Null,
                    is_error: false,
                })
            }
            async fn close(&self) -> Result<(), McpError> {
                Ok(())
            }
        }
        let desc = McpToolDesc {
            name: tool.to_string(),
            description: "dummy".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        McpToolAdapter::new(server.to_string(), desc, Arc::new(Dummy), false)
    }

    #[test]
    fn registry_merges_mcp_tools_with_namespace() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let adapters = vec![dummy_adapter("filesystem", "read_file")];
        let registry = CoreToolRegistry::new_with_mcp(&config, Some(&adapters));

        assert!(registry.get("mcp__filesystem__read_file").is_some());
        assert!(registry.get("read").is_some());
    }

    #[test]
    fn registry_keeps_builtin_when_no_mcp() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let registry = CoreToolRegistry::new_with_mcp(&config, None);
        assert!(registry.get("read").is_some());
        assert!(registry.get("mcp__anything__tool").is_none());
    }

    struct DummyTool {
        name: &'static str,
        description: &'static str,
        policy: Policy,
    }

    impl DummyTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                description: "dummy tool",
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
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
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
            _ctx: legion_runtime::ToolContext,
        ) -> Result<legion_runtime::ToolResult, legion_runtime::ToolError> {
            Ok(legion_runtime::ToolResult::ok("dummy"))
        }
    }

    #[test]
    fn register_adds_new_tool() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let mut registry = CoreToolRegistry::new(&config);
        registry.register(Arc::new(DummyTool::new("custom_tool")));

        let tool = registry.get("custom_tool").expect("tool registered");
        assert_eq!(tool.description(), "dummy tool");
        assert!(registry.names().contains(&"custom_tool".to_string()));
    }

    #[test]
    fn register_does_not_overwrite_existing_tool() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let mut registry = CoreToolRegistry::new(&config);
        let before = registry.get("read").unwrap().description().to_string();
        registry.register(Arc::new(DummyTool::new("read")));

        let after = registry.get("read").unwrap();
        assert_eq!(after.description(), before, "built-in tool must win");
        assert_ne!(after.description(), "dummy tool");
    }
}
