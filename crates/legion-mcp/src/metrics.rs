//! Metrics sink abstraction for MCP tool calls.
//!
//! Defined in `legion-mcp` so the crate can record per-call telemetry without
//! depending on the gateway. The gateway provides the concrete implementation
//! backed by its Prometheus `MetricsRegistry`.

/// Records telemetry for MCP tool invocations.
pub trait McpMetrics: Send + Sync {
    /// Record a single `tools/call` invocation for `(server, tool)`.
    fn record_call(&self, server: &str, tool: &str);
    /// Record a failed `tools/call` (transport/protocol error or `isError`)
    /// for `(server, tool)`.
    fn record_error(&self, server: &str, tool: &str);
}
