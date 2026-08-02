//! `McpManager`: loads configured servers, surfaces adapted tools, and applies
//! cross-cutting safety limits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};

use crate::adapter::{McpToolAdapter, truncate_description};
use crate::client::{McpClient, McpError, build_client};
use crate::metrics::McpMetrics;
use crate::transport::{McpServerConfig, McpTransport};

/// How long a server with a failed auth/connect is short-circuited.
const AUTH_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Default concurrency limits.
const LOCAL_PERMITS: usize = 3;
const REMOTE_PERMITS: usize = 20;

/// Result of loading configured MCP servers.
#[derive(Debug, Default)]
pub struct LoadReport {
    pub connected: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub tools: usize,
}

/// Point-in-time snapshot of a connected server, for status UIs.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    /// Configured server name.
    pub server: String,
    /// Negotiated protocol version.
    pub protocol_version: String,
    /// Capabilities the server reported during negotiation.
    pub capabilities: Value,
    /// Number of tools discovered from this server.
    pub tool_count: usize,
}

/// Concurrency limits for MCP connections.
#[derive(Debug)]
pub struct ConcurrencyLimits {
    local: Semaphore,
    remote: Semaphore,
}

impl ConcurrencyLimits {
    pub fn new(local: usize, remote: usize) -> Self {
        Self {
            local: Semaphore::new(local),
            remote: Semaphore::new(remote),
        }
    }

    pub fn default_limits() -> Self {
        Self::new(LOCAL_PERMITS, REMOTE_PERMITS)
    }

    fn permit_for(&self, transport: &McpTransport) -> &Semaphore {
        match transport {
            McpTransport::Stdio { .. } => &self.local,
            McpTransport::Http { .. } | McpTransport::Sse { .. } | McpTransport::Ws { .. } => {
                &self.remote
            }
        }
    }
}

/// Manages all configured MCP servers and the tools they expose.
pub struct McpManager {
    clients: HashMap<String, Arc<dyn McpClient>>,
    adapters: Vec<McpToolAdapter>,
    auth_cache: Mutex<HashMap<String, Instant>>,
    concurrency: ConcurrencyLimits,
    metrics: Option<Arc<dyn McpMetrics>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            adapters: Vec::new(),
            auth_cache: Mutex::new(HashMap::new()),
            concurrency: ConcurrencyLimits::default_limits(),
            metrics: None,
        }
    }

    /// Attach a metrics sink recorded by every tool discovered after this call.
    pub fn set_metrics(&mut self, metrics: Arc<dyn McpMetrics>) {
        self.metrics = Some(metrics);
    }

    fn attach_metrics(&self, adapters: Vec<McpToolAdapter>) -> Vec<McpToolAdapter> {
        match &self.metrics {
            Some(metrics) => adapters
                .into_iter()
                .map(|adapter| adapter.with_metrics(metrics.clone()))
                .collect(),
            None => adapters,
        }
    }

    /// Load all enabled servers, connecting and listing their tools. Servers
    /// that fail to connect are recorded in the returned report and added to
    /// the auth cache to short-circuit subsequent attempts.
    pub async fn load(&mut self, configs: &[McpServerConfig]) -> LoadReport {
        let mut report = LoadReport::default();
        for cfg in configs {
            if !cfg.enabled {
                continue;
            }

            if self.is_short_circuited(&cfg.name).await {
                report
                    .failed
                    .push((cfg.name.clone(), "auth cache short-circuit".to_string()));
                continue;
            }

            let _permit = self
                .concurrency
                .permit_for(&cfg.transport)
                .acquire()
                .await
                .expect("semaphore closed");

            let timeout = Duration::from_millis(cfg.connect_timeout_ms);
            let result = tokio::time::timeout(timeout, connect_server(cfg)).await;

            match result {
                Ok(Ok((client, adapters))) => {
                    self.clients.insert(cfg.name.clone(), client);
                    report.connected.push(cfg.name.clone());
                    report.tools += adapters.len();
                    let adapters = self.attach_metrics(adapters);
                    self.adapters.extend(adapters);
                }
                Ok(Err(err)) => {
                    self.record_failure(&cfg.name).await;
                    report.failed.push((cfg.name.clone(), err.to_string()));
                }
                Err(_) => {
                    self.record_failure(&cfg.name).await;
                    report
                        .failed
                        .push((cfg.name.clone(), "connect timeout".to_string()));
                }
            }
        }
        report
    }

    async fn is_short_circuited(&self, name: &str) -> bool {
        let cache = self.auth_cache.lock().await;
        if let Some(ts) = cache.get(name) {
            return ts.elapsed() < AUTH_CACHE_TTL;
        }
        false
    }

    async fn record_failure(&self, name: &str) {
        let mut cache = self.auth_cache.lock().await;
        cache.insert(name.to_string(), Instant::now());
    }

    /// Return the adapted tools discovered during [`load`].
    pub fn tools(&self) -> &[McpToolAdapter] {
        &self.adapters
    }

    /// Look up a connected client by server name.
    fn client(&self, server: &str) -> Result<&Arc<dyn McpClient>, McpError> {
        self.clients.get(server).ok_or_else(|| {
            McpError::Protocol(format!("unknown or unconnected MCP server '{server}'"))
        })
    }

    /// List a server's resources (`resources/list`, paginated).
    pub async fn list_resources(&self, server: &str) -> Result<Vec<Value>, McpError> {
        self.client(server)?.list_resources().await
    }

    /// Read a single resource from a server (`resources/read`).
    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<Value, McpError> {
        self.client(server)?.read_resource(uri).await
    }

    /// List a server's prompts (`prompts/list`, paginated).
    pub async fn list_prompts(&self, server: &str) -> Result<Vec<Value>, McpError> {
        self.client(server)?.list_prompts().await
    }

    /// Fetch a single prompt from a server (`prompts/get`).
    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<Value, McpError> {
        self.client(server)?.get_prompt(name, arguments).await
    }

    /// Snapshot every connected server for a status view, sorted by name.
    pub fn server_status(&self) -> Vec<McpServerStatus> {
        let mut statuses: Vec<McpServerStatus> = self
            .clients
            .iter()
            .map(|(name, client)| McpServerStatus {
                server: name.clone(),
                protocol_version: client.protocol_version(),
                capabilities: client.server_capabilities(),
                tool_count: self.adapters.iter().filter(|a| a.server() == name).count(),
            })
            .collect();
        statuses.sort_by(|a, b| a.server.cmp(&b.server));
        statuses
    }

    /// Close all server connections. Errors are logged but ignored.
    pub async fn shutdown_all(&self) {
        for (name, client) in &self.clients {
            if let Err(err) = client.close().await {
                tracing::warn!(server = %name, error = %err, "failed to close MCP server");
            }
        }
    }
}

async fn connect_server(
    cfg: &McpServerConfig,
) -> Result<(Arc<dyn McpClient>, Vec<McpToolAdapter>), McpError> {
    let client: Arc<dyn McpClient> = Arc::from(build_client(cfg));
    client.connect().await?;
    let tools = client.list_tools().await?;
    let adapters = tools
        .into_iter()
        .map(|mut desc| {
            desc.description = truncate_description(&desc.description);
            let auto_approved = cfg.auto_approve.iter().any(|t| t == &desc.name);
            McpToolAdapter::new(cfg.name.clone(), desc, client.clone(), auto_approved)
        })
        .collect();
    Ok((client, adapters))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_limits_match_transport() {
        let limits = ConcurrencyLimits::default_limits();
        let stdio = McpTransport::Stdio {
            command: "x".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
        };
        let http = McpTransport::Http {
            url: "http://x".to_string(),
            headers: HashMap::new(),
        };
        assert_eq!(limits.permit_for(&stdio).available_permits(), LOCAL_PERMITS);
        assert_eq!(limits.permit_for(&http).available_permits(), REMOTE_PERMITS);
    }

    #[tokio::test]
    async fn auth_cache_short_circuits_failures() {
        let manager = McpManager::new();
        manager.record_failure("bad").await;
        assert!(manager.is_short_circuited("bad").await);
        assert!(!manager.is_short_circuited("good").await);
    }

    #[test]
    fn description_truncation_caps_length() {
        let long = "a".repeat(10_000);
        let truncated = truncate_description(&long);
        assert!(truncated.len() <= crate::adapter::MAX_MCP_DESCRIPTION_LENGTH + 32);
    }

    #[tokio::test]
    async fn manager_loads_http_server_and_surfaces_namespaced_tools() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "initialize" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "protocolVersion": "2024-11-05", "capabilities": {} }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(serde_json::json!({ "method": "tools/list" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "tools": [
                        { "name": "read_file", "description": "read a file", "inputSchema": {"type": "object"} },
                        { "name": "write_file", "description": "write a file", "inputSchema": {"type": "object"} }
                    ]
                }
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let cfg = McpServerConfig {
            name: "fs".to_string(),
            transport: McpTransport::Http {
                url,
                headers: HashMap::new(),
            },
            enabled: true,
            auto_approve: vec!["read_file".to_string()],
            connect_timeout_ms: 5_000,
            protocol_version: None,
            tool_timeout_ms: 60_000,
        };

        let mut manager = McpManager::new();
        let report = manager.load(&[cfg]).await;
        assert_eq!(report.connected, vec!["fs"]);
        assert!(report.failed.is_empty());
        assert_eq!(report.tools, 2);

        let tools = manager.tools();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.qualified_name()).collect();
        assert!(names.contains(&"mcp__fs__read_file"));
        assert!(names.contains(&"mcp__fs__write_file"));

        let read = tools.iter().find(|t| t.tool_name() == "read_file").unwrap();
        assert!(read.auto_approved());
        let write = tools
            .iter()
            .find(|t| t.tool_name() == "write_file")
            .unwrap();
        assert!(!write.auto_approved());
    }

    #[tokio::test]
    async fn manager_records_failed_server_and_short_circuits() {
        let cfg = McpServerConfig {
            name: "bad".to_string(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1:1/rpc".to_string(),
                headers: HashMap::new(),
            },
            enabled: true,
            auto_approve: Vec::new(),
            connect_timeout_ms: 200,
            protocol_version: None,
            tool_timeout_ms: 60_000,
        };

        let mut manager = McpManager::new();
        let report = manager.load(std::slice::from_ref(&cfg)).await;
        assert!(report.connected.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(manager.is_short_circuited("bad").await);

        // Second load short-circuits the server without reconnecting.
        let report2 = manager.load(&[cfg]).await;
        assert!(report2.connected.is_empty());
        assert_eq!(report2.failed.len(), 1);
    }

    #[tokio::test]
    async fn manager_skips_disabled_servers() {
        let cfg = McpServerConfig {
            name: "off".to_string(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1:1/rpc".to_string(),
                headers: HashMap::new(),
            },
            enabled: false,
            auto_approve: Vec::new(),
            connect_timeout_ms: 200,
            protocol_version: None,
            tool_timeout_ms: 60_000,
        };
        let mut manager = McpManager::new();
        let report = manager.load(&[cfg]).await;
        assert!(report.connected.is_empty());
        assert!(report.failed.is_empty());
        assert!(manager.tools().is_empty());
    }

    #[tokio::test]
    async fn manager_surfaces_introspection_and_status() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rpc_result = |result: serde_json::Value| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": result
            }))
        };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "initialize" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "resources": {}, "prompts": {} }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "tools": [
                    { "name": "read_file", "description": "read", "inputSchema": {"type": "object"} }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "resources/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "resources": [{ "uri": "file:///a.txt" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "resources/read" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "contents": [{ "uri": "file:///a.txt", "text": "hi" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "prompts/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "prompts": [{ "name": "greet" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "prompts/get" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "messages": []
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let cfg = McpServerConfig {
            name: "fs".to_string(),
            transport: McpTransport::Http {
                url,
                headers: HashMap::new(),
            },
            enabled: true,
            auto_approve: Vec::new(),
            connect_timeout_ms: 5_000,
            protocol_version: None,
            tool_timeout_ms: 60_000,
        };

        let mut manager = McpManager::new();
        let report = manager.load(&[cfg]).await;
        assert_eq!(report.connected, vec!["fs"]);

        let resources = manager.list_resources("fs").await.unwrap();
        assert_eq!(resources.len(), 1);
        let contents = manager.read_resource("fs", "file:///a.txt").await.unwrap();
        assert!(contents.get("contents").is_some());
        let prompts = manager.list_prompts("fs").await.unwrap();
        assert_eq!(prompts.len(), 1);
        let prompt = manager.get_prompt("fs", "greet", None).await.unwrap();
        assert!(prompt.get("messages").is_some());

        let status = manager.server_status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].server, "fs");
        assert_eq!(status[0].protocol_version, "2024-11-05");
        assert_eq!(status[0].tool_count, 1);
        assert_eq!(
            status[0].capabilities,
            serde_json::json!({ "resources": {}, "prompts": {} })
        );
    }

    #[tokio::test]
    async fn manager_introspection_unknown_server_errors() {
        let manager = McpManager::new();
        let err = manager.list_resources("nope").await.unwrap_err();
        match err {
            McpError::Protocol(msg) => assert!(msg.contains("nope"), "got: {msg}"),
            other => panic!("expected protocol error, got {other:?}"),
        }
        let err = manager.get_prompt("nope", "greet", None).await.unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)));
        assert!(manager.server_status().is_empty());
    }
}
