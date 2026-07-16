//! Adapter that exposes a single MCP tool to the Legion tool registry.

use std::sync::Arc;

use serde_json::Value;

use crate::client::{McpClient, McpError, McpToolDesc, McpToolResult};
use crate::metrics::McpMetrics;

/// Maximum length of a tool description surfaced to the runtime.
pub const MAX_MCP_DESCRIPTION_LENGTH: usize = 2048;

/// Truncate `description` to [`MAX_MCP_DESCRIPTION_LENGTH`] characters. If a
/// multi-byte character straddles the boundary, the description is truncated at
/// the previous char boundary.
pub fn truncate_description(description: &str) -> String {
    if description.len() <= MAX_MCP_DESCRIPTION_LENGTH {
        return description.to_string();
    }
    let mut end = MAX_MCP_DESCRIPTION_LENGTH;
    while end > 0 && !description.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... (truncated)", &description[..end])
}

/// Wraps a single MCP tool together with the client used to invoke it.
///
/// The adapter is intentionally transport-agnostic: it only carries enough
/// state to describe and invoke the tool. The `legion-tools` crate wraps it in
/// an implementation of the Legion `Tool` trait so it can join the core
/// registry. Cloning is cheap because the inner client is reference counted.
#[derive(Clone)]
pub struct McpToolAdapter {
    server: String,
    desc: McpToolDesc,
    client: Arc<dyn McpClient>,
    auto_approved: bool,
    qualified_name: String,
    metrics: Option<Arc<dyn McpMetrics>>,
}

impl McpToolAdapter {
    pub fn new(
        server: impl Into<String>,
        desc: McpToolDesc,
        client: Arc<dyn McpClient>,
        auto_approved: bool,
    ) -> Self {
        let server = server.into();
        let qualified_name = format!("mcp__{}__{}", server, desc.name);
        Self {
            server,
            desc,
            client,
            auto_approved,
            qualified_name,
            metrics: None,
        }
    }

    /// Attach a metrics sink. Recorded on every [`call`](Self::call).
    pub fn with_metrics(mut self, metrics: Arc<dyn McpMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Fully qualified tool name, e.g. `mcp__filesystem__read_file`.
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Raw MCP tool name (without namespace prefix).
    pub fn tool_name(&self) -> &str {
        &self.desc.name
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn description(&self) -> &str {
        &self.desc.description
    }

    pub fn input_schema(&self) -> &Value {
        &self.desc.input_schema
    }

    /// Whether the server config declares this tool in `autoApprove`.
    pub fn auto_approved(&self) -> bool {
        self.auto_approved
    }

    /// Invoke the underlying MCP `tools/call`. Uses the client's resilient
    /// variant, which reconnects and retries once on session-expiry errors.
    /// Records one call (and one error on failure/`isError`) when a metrics sink
    /// is attached.
    pub async fn call(&self, args: Value) -> Result<McpToolResult, McpError> {
        if let Some(metrics) = &self.metrics {
            metrics.record_call(&self.server, &self.desc.name);
        }
        let outcome = self.client.call_tool_resilient(&self.desc.name, args).await;
        let failed = match &outcome {
            Ok(result) => result.is_error,
            Err(_) => true,
        };
        if failed {
            if let Some(metrics) = &self.metrics {
                metrics.record_error(&self.server, &self.desc.name);
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_name_uses_namespace() {
        let desc = McpToolDesc {
            name: "read_file".to_string(),
            description: "read a file".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        // The client is unused for this test; build a dummy one.
        struct Dummy;
        #[async_trait::async_trait]
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
        let adapter = McpToolAdapter::new("filesystem", desc, Arc::new(Dummy), false);
        assert_eq!(adapter.qualified_name(), "mcp__filesystem__read_file");
        assert_eq!(adapter.tool_name(), "read_file");
        assert!(!adapter.auto_approved());
    }

    #[test]
    fn truncation_keeps_short_text() {
        assert_eq!(truncate_description("hello"), "hello");
    }

    #[test]
    fn truncation_caps_long_text() {
        let long = "a".repeat(10_000);
        let truncated = truncate_description(&long);
        assert!(truncated.starts_with(&"a".repeat(MAX_MCP_DESCRIPTION_LENGTH)));
        assert!(truncated.ends_with("(truncated)"));
    }

    #[test]
    fn truncation_respects_char_boundary() {
        let long = "🦀".repeat(10_000);
        let truncated = truncate_description(&long);
        assert!(truncated.ends_with("(truncated)"));
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct CountingMetrics {
        calls: AtomicU64,
        errors: AtomicU64,
    }

    impl McpMetrics for CountingMetrics {
        fn record_call(&self, _server: &str, _tool: &str) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
        fn record_error(&self, _server: &str, _tool: &str) {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct ScriptedClient {
        result: Result<McpToolResult, McpError>,
    }

    #[async_trait::async_trait]
    impl McpClient for ScriptedClient {
        fn server_name(&self) -> &str {
            "scripted"
        }
        async fn connect(&self) -> Result<(), McpError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
            Ok(Vec::new())
        }
        async fn call_tool(&self, _: &str, _: Value) -> Result<McpToolResult, McpError> {
            match &self.result {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(McpError::Protocol(e.to_string())),
            }
        }
        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    fn desc() -> McpToolDesc {
        McpToolDesc {
            name: "ping".to_string(),
            description: "ping".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[tokio::test]
    async fn metrics_records_successful_call() {
        let metrics = Arc::new(CountingMetrics::default());
        let client = Arc::new(ScriptedClient {
            result: Ok(McpToolResult {
                content: Value::Null,
                is_error: false,
            }),
        });
        let adapter =
            McpToolAdapter::new("fs", desc(), client, false).with_metrics(metrics.clone());
        let _ = adapter.call(serde_json::json!({})).await.unwrap();
        assert_eq!(metrics.calls.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.errors.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn metrics_records_error_on_is_error() {
        let metrics = Arc::new(CountingMetrics::default());
        let client = Arc::new(ScriptedClient {
            result: Ok(McpToolResult {
                content: serde_json::json!("boom"),
                is_error: true,
            }),
        });
        let adapter =
            McpToolAdapter::new("fs", desc(), client, false).with_metrics(metrics.clone());
        let _ = adapter.call(serde_json::json!({})).await.unwrap();
        assert_eq!(metrics.calls.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.errors.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn metrics_records_error_on_rpc_error() {
        let metrics = Arc::new(CountingMetrics::default());
        let client = Arc::new(ScriptedClient {
            result: Err(McpError::Protocol("fail".to_string())),
        });
        let adapter =
            McpToolAdapter::new("fs", desc(), client, false).with_metrics(metrics.clone());
        let _ = adapter.call(serde_json::json!({})).await;
        assert_eq!(metrics.calls.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.errors.load(Ordering::Relaxed), 1);
    }
}
