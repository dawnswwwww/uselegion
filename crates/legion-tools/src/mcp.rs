//! Bridge between [`legion_mcp::McpToolAdapter`] and the Legion [`Tool`] trait.

use async_trait::async_trait;
use legion_mcp::McpToolAdapter;
use legion_runtime::tools::{Policy, Tool, ToolContext, ToolError, ToolResult};

use crate::policy::Approval;

/// Wraps an MCP tool adapter so it can join the core tool registry.
pub struct McpTool {
    adapter: McpToolAdapter,
    policy: Policy,
}

impl McpTool {
    pub fn new(adapter: McpToolAdapter) -> Self {
        let approval = if adapter.auto_approved() {
            Approval::Off
        } else {
            Approval::Required
        };
        let policy = Policy {
            approval,
            permission_mode: None,
            allow_from: Vec::new(),
            workspace_only: false,
        };
        Self { adapter, policy }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        self.adapter.qualified_name()
    }

    fn description(&self) -> &str {
        self.adapter.description()
    }

    fn schema(&self) -> serde_json::Value {
        self.adapter.input_schema().clone()
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let result = self
            .adapter
            .call(params)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let content = match &result.content {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other)
                .unwrap_or_else(|_| "<unserializable mcp result>".to_string()),
        };
        if result.is_error {
            Ok(ToolResult::error(content))
        } else {
            Ok(ToolResult::ok(content))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_mcp::client::{McpClient, McpError, McpToolDesc, McpToolResult};
    use serde_json::Value;
    use std::sync::Arc;

    struct EchoClient;

    #[async_trait]
    impl McpClient for EchoClient {
        fn server_name(&self) -> &str {
            "echo"
        }
        async fn connect(&self) -> Result<(), McpError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
            Ok(Vec::new())
        }
        async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError> {
            Ok(McpToolResult {
                content: serde_json::json!({ "echoed": { "name": name, "args": args } }),
                is_error: false,
            })
        }
        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn mcp_tool_invokes_adapter_and_returns_json() {
        let desc = McpToolDesc {
            name: "ping".to_string(),
            description: "ping".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let adapter = McpToolAdapter::new("echo", desc, Arc::new(EchoClient), false);
        let tool = McpTool::new(adapter);

        assert_eq!(tool.name(), "mcp__echo__ping");
        assert_eq!(tool.policy().approval, Approval::Required);

        let result = tool
            .execute(
                serde_json::json!({"x": 1}),
                ToolContext {
                    workspace: std::path::PathBuf::from("/tmp"),
                    session_id: "s".to_string(),
                    agent_id: "a".to_string(),
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
                },
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("ping"));
    }

    #[tokio::test]
    async fn auto_approved_mcp_tool_has_off_policy() {
        let desc = McpToolDesc {
            name: "read_file".to_string(),
            description: "read".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let adapter = McpToolAdapter::new("fs", desc, Arc::new(EchoClient), true);
        let tool = McpTool::new(adapter);
        assert_eq!(tool.policy().approval, Approval::Off);
    }
}
