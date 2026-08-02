//! MCP client trait plus stdio, http, sse and websocket transports.
//!
//! All transports negotiate the protocol version during `initialize`, falling
//! back from [`crate::version::LATEST_VERSION`] to older revisions (see
//! [`crate::version`]). When the stateless 2026-07-28 core is negotiated,
//! requests become self-describing (`_meta` client identity, `Mcp-Method` /
//! `Mcp-Name` / `MCP-Protocol-Version` HTTP headers) and the
//! `notifications/initialized` handshake is skipped. The http transport also
//! speaks the 2025-03-26 streamable HTTP profile (`Accept: application/json,
//! text/event-stream`, SSE-framed responses, `Mcp-Session-Id`).

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::{Sink, SinkExt, StreamExt};
use legion_core::jsonrpc;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

use crate::transport::McpTransport;
use crate::version::{
    ProtocolState, STATELESS_VERSION, SUPPORTED_VERSIONS, is_stateless, requires_version_header,
};

/// Client identity reported in `initialize` params and stateless `_meta`.
const CLIENT_NAME: &str = "legion";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default per-request timeout for `tools/call`.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_millis(60_000);

/// Options threaded from `McpServerConfig` into every transport client.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Pin a protocol version, skipping the negotiation fallback chain.
    pub pinned_protocol_version: Option<String>,
    /// Per-request timeout for `tools/call`.
    pub tool_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            pinned_protocol_version: None,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
        }
    }
}

/// Description of a tool exposed by an MCP server.
#[derive(Debug, Clone)]
pub struct McpToolDesc {
    /// Raw tool name (without namespace prefix).
    pub name: String,
    /// Tool description. Truncated to `MAX_MCP_DESCRIPTION_LENGTH` by the
    /// manager before surfacing to the runtime.
    pub description: String,
    /// JSON Schema describing the tool input.
    pub input_schema: Value,
    /// Raw tool annotations object (`readOnlyHint`, `destructiveHint`, ...)
    /// as declared by the server, when present.
    pub annotations: Option<Value>,
    /// JSON Schema describing the tool's structured output, when declared.
    pub output_schema: Option<Value>,
}

/// Result returned by an MCP `tools/call`.
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: Value,
    pub is_error: bool,
    /// The result's `structuredContent` payload, when the server provided one.
    pub structured_content: Option<Value>,
}

/// Errors produced by MCP clients.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("server returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl From<jsonrpc::RpcError> for McpError {
    fn from(err: jsonrpc::RpcError) -> Self {
        McpError::Rpc {
            code: err.code,
            message: err.message,
        }
    }
}

/// Client talking to a single MCP server.
#[async_trait]
pub trait McpClient: Send + Sync {
    fn server_name(&self) -> &str;
    async fn connect(&self) -> Result<(), McpError>;

    /// Send a JSON-RPC request and await the matching response.
    ///
    /// Each transport implements this single primitive; `initialize`,
    /// `list_tools` and `call_tool` are provided on top of it. The default
    /// errors so stub clients that implement the high-level methods directly
    /// keep working.
    async fn request(&self, _method: &str, _params: Value) -> Result<Value, McpError> {
        Err(McpError::Protocol(
            "transport does not implement request".to_string(),
        ))
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
        Err(McpError::Protocol(
            "transport does not implement notify".to_string(),
        ))
    }

    /// The negotiated protocol version (the latest supported one before
    /// negotiation completes).
    fn protocol_version(&self) -> String {
        crate::version::LATEST_VERSION.to_string()
    }

    /// Capabilities the server reported during negotiation (`{}` before).
    fn server_capabilities(&self) -> Value {
        serde_json::json!({})
    }

    /// Record a negotiation outcome (also used to mark the fallback-chain
    /// attempt in progress, with empty capabilities).
    fn set_negotiated(&self, _version: String, _capabilities: Value) {}

    /// Config-pinned protocol version, if any. A pin skips the fallback chain.
    fn pinned_protocol_version(&self) -> Option<String> {
        None
    }

    /// Whether this transport may fall back to the stateless `server/discover`
    /// RPC when `initialize` is unknown (HTTP-family transports only).
    fn supports_stateless_discover(&self) -> bool {
        false
    }

    /// Per-request timeout applied to `tools/call`.
    fn tool_timeout(&self) -> Duration {
        DEFAULT_TOOL_TIMEOUT
    }

    /// Perform the MCP handshake, negotiating the protocol version.
    ///
    /// Without a config pin, each version from
    /// [`SUPPORTED_VERSIONS`] is tried newest-first: a JSON-RPC error or HTTP
    /// 4xx rejection moves to the next older version, while a successful
    /// result adopts the server-returned `protocolVersion` (permissively, even
    /// when older than our minimum) and stores the server `capabilities`.
    /// When `initialize` is unknown (`-32601`) on an HTTP-family transport,
    /// the stateless `server/discover` RPC takes over. The
    /// `notifications/initialized` notification is only sent for pre-2026-07-28
    /// (stateful) versions.
    async fn initialize(&self) -> Result<(), McpError> {
        let pinned = self.pinned_protocol_version();
        let candidates: Vec<String> = match &pinned {
            Some(version) => vec![version.clone()],
            None => SUPPORTED_VERSIONS.iter().map(|v| v.to_string()).collect(),
        };
        let mut last_err: Option<McpError> = None;
        for version in candidates {
            // Advertise the attempt as the current version so stateless
            // `_meta`/headers only decorate 2026-07-28 attempts.
            self.set_negotiated(version.clone(), serde_json::json!({}));
            match self
                .request("initialize", initialize_params(&version))
                .await
            {
                Ok(result) => {
                    let adopted = result
                        .get("protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or(&version)
                        .to_string();
                    let capabilities = result
                        .get("capabilities")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    self.set_negotiated(adopted.clone(), capabilities);
                    tracing::debug!(
                        server = %self.server_name(),
                        version = %adopted,
                        "mcp protocol version negotiated"
                    );
                    if !is_stateless(&adopted) {
                        self.notify("notifications/initialized", serde_json::json!({}))
                            .await?;
                    }
                    return Ok(());
                }
                Err(err) => {
                    if matches!(err, McpError::Rpc { code: -32601, .. })
                        && self.supports_stateless_discover()
                    {
                        return self.discover_server().await;
                    }
                    if pinned.is_some() || !is_version_rejection(&err) {
                        return Err(err);
                    }
                    tracing::debug!(
                        server = %self.server_name(),
                        version = %version,
                        error = %err,
                        "mcp server rejected protocol version; trying an older one"
                    );
                    last_err = Some(err);
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| McpError::Protocol("no protocol versions to negotiate".to_string())))
    }

    /// Fall back to the stateless `server/discover` RPC when `initialize` is
    /// not implemented (2026-07-28 core servers). Negotiates the stateless
    /// version and skips `notifications/initialized`.
    async fn discover_server(&self) -> Result<(), McpError> {
        let result = self
            .request("server/discover", serde_json::json!({}))
            .await?;
        let capabilities = result
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| result.clone());
        self.set_negotiated(STATELESS_VERSION.to_string(), capabilities);
        tracing::debug!(
            server = %self.server_name(),
            "mcp server is stateless; discovered via server/discover"
        );
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
        let tools = crate::introspect::paginate(self, "tools/list", "tools").await?;
        parse_tool_list(tools)
    }

    /// List the server's resources (`resources/list`), following `nextCursor`
    /// pagination. Returns an empty list without hitting the wire when the
    /// server declared a non-empty capability map without `resources`;
    /// servers that reported no capabilities at all are tried permissively
    /// (see [`crate::introspect::capability_supported`]).
    async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
        crate::introspect::list_resources(self).await
    }

    /// Read a single resource (`resources/read`). Returns
    /// [`McpError::Protocol`] when the server did not declare the `resources`
    /// capability.
    async fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
        crate::introspect::read_resource(self, uri).await
    }

    /// List the server's prompts (`prompts/list`), following `nextCursor`
    /// pagination. Capability-gated like [`list_resources`](Self::list_resources).
    async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
        crate::introspect::list_prompts(self).await
    }

    /// Fetch a single prompt (`prompts/get`). Returns
    /// [`McpError::Protocol`] when the server did not declare the `prompts`
    /// capability.
    async fn get_prompt(&self, name: &str, arguments: Option<Value>) -> Result<Value, McpError> {
        crate::introspect::get_prompt(self, name, arguments).await
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError> {
        let timeout = self.tool_timeout();
        let call = self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": args }),
        );
        let result = match tokio::time::timeout(timeout, call).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(McpError::Timeout(format!(
                    "tools/call '{name}' exceeded {}ms",
                    timeout.as_millis()
                )));
            }
        };
        Ok(parse_tool_result(result))
    }

    async fn close(&self) -> Result<(), McpError>;

    /// Call a tool, reconnecting and retrying once when the server reports that
    /// the session has expired (JSON-RPC `-32001` / HTTP 404 session expiry).
    ///
    /// Implemented as a default method so individual transports do not need to
    /// duplicate the logic.
    async fn call_tool_resilient(
        &self,
        name: &str,
        args: Value,
    ) -> Result<McpToolResult, McpError> {
        match self.call_tool(name, args.clone()).await {
            Err(err) if is_session_expired(&err) => {
                tracing::warn!(
                    server = %self.server_name(),
                    error = %err,
                    "mcp session expired; reconnecting and retrying once"
                );
                self.connect().await?;
                self.call_tool(name, args).await
            }
            other => other,
        }
    }
}

/// Whether an error signals an expired MCP session that a reconnect can fix.
fn is_session_expired(err: &McpError) -> bool {
    matches!(err, McpError::Rpc { code: -32001, .. })
}

/// Pending JSON-RPC requests keyed by id, routed to per-request oneshots.
type PendingMap = Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>;

/// Allocate a fresh id and register a oneshot receiver for its response.
async fn register_request(
    pending: &PendingMap,
    next_id: &AtomicU64,
) -> (u64, oneshot::Receiver<Result<Value, McpError>>) {
    let id = jsonrpc::next_id(next_id);
    let (tx, rx) = oneshot::channel();
    pending.lock().await.insert(id, tx);
    (id, rx)
}

/// Parse a JSON-RPC message into its `result` or an `Rpc` error.
fn parse_jsonrpc(msg: &Value) -> Result<Value, McpError> {
    jsonrpc::parse_result(msg).map_err(McpError::from)
}

/// Route a JSON-RPC response to the waiter matching its id. Notifications
/// (messages without an id) are ignored.
async fn dispatch_jsonrpc(pending: &PendingMap, msg: Value) {
    let Some(id) = msg.get("id").and_then(|i| i.as_u64()) else {
        return;
    };
    let outcome = parse_jsonrpc(&msg);
    let sender = pending.lock().await.remove(&id);
    if let Some(tx) = sender {
        let _ = tx.send(outcome);
    }
}

/// Fail every pending request (used when the underlying connection closes).
async fn fail_all_pending(pending: &PendingMap) {
    let mut guard = pending.lock().await;
    for (_, tx) in guard.drain() {
        let _ = tx.send(Err(McpError::Transport("connection closed".to_string())));
    }
}

/// Decode paginated `tools/list` entries into tool descriptors.
fn parse_tool_list(tools: Vec<Value>) -> Result<Vec<McpToolDesc>, McpError> {
    let mut descs = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| McpError::Protocol("tool missing name".to_string()))?
            .to_string();
        let description = tool
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let input_schema = tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        let annotations = tool.get("annotations").cloned();
        let output_schema = tool.get("outputSchema").cloned();
        descs.push(McpToolDesc {
            name,
            description,
            input_schema,
            annotations,
            output_schema,
        });
    }
    Ok(descs)
}

/// Decode a `tools/call` result.
fn parse_tool_result(result: Value) -> McpToolResult {
    let is_error = result
        .get("isError")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let content = result.get("content").cloned().unwrap_or(Value::Null);
    let structured_content = result.get("structuredContent").cloned();
    McpToolResult {
        content,
        is_error,
        structured_content,
    }
}

/// Whether an `initialize` failure suggests the server does not speak the
/// advertised protocol version and the next older one should be tried.
fn is_version_rejection(err: &McpError) -> bool {
    match err {
        // -32601 means `initialize` itself is unknown; that does not change
        // with the version and is handled by the `server/discover` fallback.
        McpError::Rpc { code, .. } => *code != -32601,
        // `classify_error_response` renders HTTP rejections as
        // "server returned status <code>"; a 4xx here means the request
        // shape (version, headers) was rejected.
        McpError::Transport(msg) => msg.contains("server returned status 4"),
        _ => false,
    }
}

/// Merge the stateless-mode `_meta` block (client identity and client
/// capabilities) into request params. Existing `_meta` keys the caller set
/// are preserved. No-op for pre-2026-07-28 versions and non-object params.
fn inject_stateless_meta(params: Value, version: &str) -> Value {
    if !is_stateless(version) {
        return params;
    }
    let mut obj = match params {
        Value::Object(obj) => obj,
        other => return other,
    };
    let meta = obj.entry("_meta").or_insert_with(|| serde_json::json!({}));
    if let Value::Object(meta) = meta {
        meta.entry("io.modelcontextprotocol/clientInfo".to_string())
            .or_insert_with(|| serde_json::json!({"name": CLIENT_NAME, "version": CLIENT_VERSION}));
        meta.entry("io.modelcontextprotocol/clientCapabilities".to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    Value::Object(obj)
}

/// Standard `initialize` params shared by every transport.
fn initialize_params(version: &str) -> Value {
    serde_json::json!({
        "protocolVersion": version,
        "capabilities": {},
        "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION }
    })
}

/// Resolve an SSE `endpoint` event payload against the SSE URL. Absolute
/// payloads are returned unchanged; relative ones are joined to `base`.
fn resolve_endpoint(base: &str, data: &str) -> String {
    if let Ok(base_url) = reqwest::Url::parse(base) {
        if let Ok(joined) = base_url.join(data) {
            return joined.to_string();
        }
    }
    data.to_string()
}

/// Classify a non-2xx HTTP response into an [`McpError`]. Surfaces OAuth
/// step-up (401 + `WWW-Authenticate`) and JSON-RPC errors such as session
/// expiry (404 + `-32001`).
async fn classify_error_response(response: reqwest::Response, server_name: &str) -> McpError {
    let status = response.status();
    if status.as_u16() == 401 && response.headers().get("www-authenticate").is_some() {
        tracing::warn!(
            server = %server_name,
            "mcp server requires OAuth step-up (401 + WWW-Authenticate)"
        );
        return McpError::Transport("oauth step-up required".to_string());
    }
    let body = response.text().await.unwrap_or_default();
    if let Ok(msg) = serde_json::from_str::<Value>(&body) {
        if let Err(rpc) = jsonrpc::parse_result(&msg) {
            return rpc.into();
        }
    }
    McpError::Transport(format!("server returned status {status}"))
}

/// Shared reqwest client used by the HTTP-based transports (proxy bypassed).
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Shared state for stdio clients.
struct StdioState {
    stdin: ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

/// MCP client backed by a local subprocess speaking JSON-RPC over stdio.
pub struct StdioMcpClient {
    server_name: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    next_id: AtomicU64,
    state: Mutex<Option<StdioState>>,
    child: Mutex<Option<Child>>,
    protocol: ProtocolState,
    tool_timeout: Duration,
}

impl StdioMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
        options: ClientOptions,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            command: command.into(),
            args,
            env,
            next_id: AtomicU64::new(1),
            state: Mutex::new(None),
            child: Mutex::new(None),
            protocol: ProtocolState::new(options.pinned_protocol_version),
            tool_timeout: options.tool_timeout,
        }
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn protocol_version(&self) -> String {
        self.protocol.version()
    }

    fn server_capabilities(&self) -> Value {
        self.protocol.capabilities()
    }

    fn set_negotiated(&self, version: String, capabilities: Value) {
        self.protocol.set_negotiated(version, capabilities);
    }

    fn pinned_protocol_version(&self) -> Option<String> {
        self.protocol.pinned()
    }

    fn tool_timeout(&self) -> Duration {
        self.tool_timeout
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = jsonrpc::next_id(&self.next_id);
        let params = inject_stateless_meta(params, &self.protocol.version());
        let request = jsonrpc::build_request(id, method, params);

        let mut guard = self.state.lock().await;
        let state = guard
            .as_mut()
            .ok_or_else(|| McpError::Protocol("client is not connected".to_string()))?;

        let line = serde_json::to_string(&request)?;
        state.stdin.write_all(line.as_bytes()).await?;
        state.stdin.write_all(b"\n").await?;
        state.stdin.flush().await?;

        // Read lines until we find the response matching this id. Notifications
        // (no id) are skipped.
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = state.stdout.read_line(&mut buf).await?;
            if n == 0 {
                return Err(McpError::Transport("server closed stdout".to_string()));
            }
            let msg: Value = serde_json::from_str(buf.trim())?;
            if msg.get("id").and_then(|i| i.as_u64()) != Some(id) {
                continue;
            }
            return parse_jsonrpc(&msg);
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = jsonrpc::build_notification(method, params);
        let mut guard = self.state.lock().await;
        let state = guard
            .as_mut()
            .ok_or_else(|| McpError::Protocol("client is not connected".to_string()))?;
        let line = serde_json::to_string(&msg)?;
        state.stdin.write_all(line.as_bytes()).await?;
        state.stdin.write_all(b"\n").await?;
        state.stdin.flush().await?;
        Ok(())
    }

    async fn connect(&self) -> Result<(), McpError> {
        let mut guard = self.child.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .envs(&self.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Transport(format!("failed to spawn '{}': {e}", self.command)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdout".to_string()))?;
        let stdout = BufReader::new(stdout);
        let mut state = self.state.lock().await;
        *state = Some(StdioState { stdin, stdout });
        *guard = Some(child);
        drop(state);
        drop(guard);

        self.initialize().await
    }

    async fn close(&self) -> Result<(), McpError> {
        let mut child = self.child.lock().await;
        if let Some(mut c) = child.take() {
            let _ = c.kill().await;
            let _ = c.wait().await;
        }
        let mut state = self.state.lock().await;
        *state = None;
        Ok(())
    }
}

/// MCP client backed by an HTTP endpoint that accepts JSON-RPC POST requests.
///
/// Speaks the 2025-03-26 streamable HTTP profile: every POST offers
/// `Accept: application/json, text/event-stream`, SSE-framed responses are
/// parsed back into JSON-RPC messages, and an `Mcp-Session-Id` returned by
/// `initialize` is resent on subsequent requests (unless the stateless
/// 2026-07-28 core was negotiated, which retires session ids and instead
/// routes on `Mcp-Method` / `Mcp-Name` / `MCP-Protocol-Version` headers).
pub struct HttpMcpClient {
    server_name: String,
    url: String,
    headers: HashMap<String, String>,
    next_id: AtomicU64,
    client: reqwest::Client,
    protocol: ProtocolState,
    tool_timeout: Duration,
    session_id: std::sync::Mutex<Option<String>>,
}

impl HttpMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        url: impl Into<String>,
        headers: HashMap<String, String>,
        options: ClientOptions,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            url: url.into(),
            headers,
            next_id: AtomicU64::new(1),
            client: http_client(),
            protocol: ProtocolState::new(options.pinned_protocol_version),
            tool_timeout: options.tool_timeout,
            session_id: std::sync::Mutex::new(None),
        }
    }

    fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_session_id(&self, session_id: String) {
        *self
            .session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(session_id);
    }

    /// Apply the headers required for the negotiated protocol version:
    /// stateless routing headers for 2026-07-28, `MCP-Protocol-Version` on
    /// post-initialize requests for 2025-03-26+, and the captured session id.
    fn decorate(
        &self,
        mut req: reqwest::RequestBuilder,
        method: &str,
        params: &Value,
    ) -> reqwest::RequestBuilder {
        req = req.header("Accept", "application/json, text/event-stream");
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        let version = self.protocol.version();
        if is_stateless(&version) {
            req = req
                .header("MCP-Protocol-Version", STATELESS_VERSION)
                .header("Mcp-Method", method);
            if method == "tools/call" {
                if let Some(name) = params.get("name").and_then(Value::as_str) {
                    req = req.header("Mcp-Name", name);
                }
            }
        } else {
            if method != "initialize" && requires_version_header(&version) {
                req = req.header("MCP-Protocol-Version", version);
            }
            if let Some(session_id) = self.session_id() {
                req = req.header("Mcp-Session-Id", session_id);
            }
        }
        req
    }

    /// Read a streamable-HTTP SSE response, returning the JSON-RPC message
    /// whose id matches the request. Other events are skipped.
    async fn read_sse_response(
        &self,
        response: reqwest::Response,
        id: u64,
    ) -> Result<Value, McpError> {
        let stream = response.bytes_stream().eventsource();
        futures::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            let event = item
                .map_err(|err| McpError::Transport(format!("sse response stream failed: {err}")))?;
            if event.event != "message" {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            if msg.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return parse_jsonrpc(&msg);
            }
        }
        Err(McpError::Transport(
            "sse response stream ended without a matching response".to_string(),
        ))
    }
}

#[async_trait]
impl McpClient for HttpMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn protocol_version(&self) -> String {
        self.protocol.version()
    }

    fn server_capabilities(&self) -> Value {
        self.protocol.capabilities()
    }

    fn set_negotiated(&self, version: String, capabilities: Value) {
        self.protocol.set_negotiated(version, capabilities);
    }

    fn pinned_protocol_version(&self) -> Option<String> {
        self.protocol.pinned()
    }

    fn supports_stateless_discover(&self) -> bool {
        true
    }

    fn tool_timeout(&self) -> Duration {
        self.tool_timeout
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = jsonrpc::next_id(&self.next_id);
        let params = inject_stateless_meta(params, &self.protocol.version());
        let request = jsonrpc::build_request(id, method, params.clone());
        let req = self.client.post(&self.url).json(&request);
        let req = self.decorate(req, method, &params);
        let response = req.send().await?;
        if method == "initialize" {
            if let Some(session_id) = response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
            {
                self.set_session_id(session_id.to_string());
            }
        }
        if !response.status().is_success() {
            return Err(classify_error_response(response, &self.server_name).await);
        }
        let is_event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|content_type| content_type.starts_with("text/event-stream"))
            .unwrap_or(false);
        if is_event_stream {
            self.read_sse_response(response, id).await
        } else {
            let msg: Value = response.json().await?;
            parse_jsonrpc(&msg)
        }
    }

    /// POST a notification. The response carries no result and its status is
    /// ignored so plain POST servers that 404 unknown methods keep working.
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = jsonrpc::build_notification(method, params.clone());
        let req = self.client.post(&self.url).json(&msg);
        let req = self.decorate(req, method, &params);
        match req.send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::debug!(
                    server = %self.server_name(),
                    status = %response.status(),
                    "mcp server rejected notification; ignoring"
                );
            }
            Err(err) => {
                tracing::debug!(
                    server = %self.server_name(),
                    error = %err,
                    "mcp notification failed; ignoring"
                );
            }
        }
        Ok(())
    }

    async fn connect(&self) -> Result<(), McpError> {
        self.initialize().await
    }

    async fn close(&self) -> Result<(), McpError> {
        Ok(())
    }
}

/// Boxed WebSocket write half used by [`WsMcpClient`].
type WsSink = Box<dyn Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin>;

/// MCP client backed by a single bidirectional WebSocket connection.
pub struct WsMcpClient {
    server_name: String,
    url: String,
    headers: HashMap<String, String>,
    next_id: AtomicU64,
    pending: Arc<PendingMap>,
    write: Mutex<Option<WsSink>>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    protocol: ProtocolState,
    tool_timeout: Duration,
}

impl WsMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        url: impl Into<String>,
        headers: HashMap<String, String>,
        options: ClientOptions,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            url: url.into(),
            headers,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            write: Mutex::new(None),
            reader: Mutex::new(None),
            protocol: ProtocolState::new(options.pinned_protocol_version),
            tool_timeout: options.tool_timeout,
        }
    }
}

fn map_ws_connect_error(err: tokio_tungstenite::tungstenite::Error, server_name: &str) -> McpError {
    if let tokio_tungstenite::tungstenite::Error::Http(resp) = &err {
        if resp.status().as_u16() == 401 && resp.headers().get("www-authenticate").is_some() {
            tracing::warn!(
                server = %server_name,
                "mcp server requires OAuth step-up (ws 401 + WWW-Authenticate)"
            );
            return McpError::Transport("oauth step-up required".to_string());
        }
    }
    McpError::Transport(format!("ws connect failed: {err}"))
}

#[async_trait]
impl McpClient for WsMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn protocol_version(&self) -> String {
        self.protocol.version()
    }

    fn server_capabilities(&self) -> Value {
        self.protocol.capabilities()
    }

    fn set_negotiated(&self, version: String, capabilities: Value) {
        self.protocol.set_negotiated(version, capabilities);
    }

    fn pinned_protocol_version(&self) -> Option<String> {
        self.protocol.pinned()
    }

    fn tool_timeout(&self) -> Duration {
        self.tool_timeout
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let (id, rx) = register_request(&self.pending, &self.next_id).await;
        let params = inject_stateless_meta(params, &self.protocol.version());
        let msg = jsonrpc::build_request(id, method, params);
        let text = serde_json::to_string(&msg)?;
        let mut guard = self.write.lock().await;
        let write = guard
            .as_mut()
            .ok_or_else(|| McpError::Protocol("ws client is not connected".to_string()))?;
        if let Err(err) = write.send(Message::Text(text.into())).await {
            drop(guard);
            self.pending.lock().await.remove(&id);
            return Err(McpError::Transport(format!("ws send failed: {err}")));
        }
        drop(guard);
        match rx.await {
            Ok(res) => res,
            Err(_) => Err(McpError::Transport("ws connection closed".to_string())),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = jsonrpc::build_notification(method, params);
        let text = serde_json::to_string(&msg)?;
        let mut guard = self.write.lock().await;
        let write = guard
            .as_mut()
            .ok_or_else(|| McpError::Protocol("ws client is not connected".to_string()))?;
        write
            .send(Message::Text(text.into()))
            .await
            .map_err(|err| McpError::Transport(format!("ws send failed: {err}")))?;
        Ok(())
    }

    async fn connect(&self) -> Result<(), McpError> {
        {
            let guard = self.reader.lock().await;
            if guard.is_some() {
                return Ok(());
            }
        }
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|err| McpError::Transport(format!("invalid ws url: {err}")))?;
        for (key, value) in &self.headers {
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|err| {
                McpError::Transport(format!("invalid ws header name '{key}': {err}"))
            })?;
            let header_value = HeaderValue::from_str(value).map_err(|err| {
                McpError::Transport(format!("invalid ws header value for '{key}': {err}"))
            })?;
            request.headers_mut().insert(name, header_value);
        }
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|err| map_ws_connect_error(err, &self.server_name))?;
        let (write, read) = ws.split();
        let write: WsSink = Box::new(write);
        *self.write.lock().await = Some(write);

        let pending = self.pending.clone();
        let handle = tokio::spawn(async move {
            let mut read = read;
            while let Some(item) = read.next().await {
                match item {
                    Ok(Message::Text(text)) => {
                        if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                            dispatch_jsonrpc(&pending, msg).await;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            fail_all_pending(&pending).await;
        });
        *self.reader.lock().await = Some(handle);

        self.initialize().await
    }

    async fn close(&self) -> Result<(), McpError> {
        if let Some(handle) = self.reader.lock().await.take() {
            handle.abort();
        }
        *self.write.lock().await = None;
        fail_all_pending(&self.pending).await;
        Ok(())
    }
}

/// MCP client backed by the MCP Server-Sent Events transport: a long-lived GET
/// stream delivers responses while requests are POSTed to an endpoint announced
/// by the stream.
pub struct SseMcpClient {
    server_name: String,
    url: String,
    headers: HashMap<String, String>,
    next_id: AtomicU64,
    client: reqwest::Client,
    pending: Arc<PendingMap>,
    post_url: Mutex<Option<String>>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    protocol: ProtocolState,
    tool_timeout: Duration,
}

impl SseMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        url: impl Into<String>,
        headers: HashMap<String, String>,
        options: ClientOptions,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            url: url.into(),
            headers,
            next_id: AtomicU64::new(1),
            client: http_client(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            post_url: Mutex::new(None),
            reader: Mutex::new(None),
            protocol: ProtocolState::new(options.pinned_protocol_version),
            tool_timeout: options.tool_timeout,
        }
    }

    async fn post_endpoint(&self, body: Value) -> Result<reqwest::Response, McpError> {
        let post_url = {
            let guard = self.post_url.lock().await;
            guard
                .clone()
                .ok_or_else(|| McpError::Protocol("sse client is not connected".to_string()))?
        };
        let mut req = self.client.post(&post_url).json(&body);
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        let response = req.send().await?;
        if !response.status().is_success() {
            return Err(classify_error_response(response, &self.server_name).await);
        }
        Ok(response)
    }
}

#[async_trait]
impl McpClient for SseMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn protocol_version(&self) -> String {
        self.protocol.version()
    }

    fn server_capabilities(&self) -> Value {
        self.protocol.capabilities()
    }

    fn set_negotiated(&self, version: String, capabilities: Value) {
        self.protocol.set_negotiated(version, capabilities);
    }

    fn pinned_protocol_version(&self) -> Option<String> {
        self.protocol.pinned()
    }

    fn supports_stateless_discover(&self) -> bool {
        true
    }

    fn tool_timeout(&self) -> Duration {
        self.tool_timeout
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let (id, rx) = register_request(&self.pending, &self.next_id).await;
        let params = inject_stateless_meta(params, &self.protocol.version());
        let msg = jsonrpc::build_request(id, method, params);
        if let Err(err) = self.post_endpoint(msg).await {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }
        match rx.await {
            Ok(res) => res,
            Err(_) => Err(McpError::Transport("sse connection closed".to_string())),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = jsonrpc::build_notification(method, params);
        self.post_endpoint(msg).await?;
        Ok(())
    }

    async fn connect(&self) -> Result<(), McpError> {
        {
            let guard = self.reader.lock().await;
            if guard.is_some() {
                return Ok(());
            }
        }
        let mut req = self
            .client
            .get(&self.url)
            .header("Accept", "text/event-stream");
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        let response = req.send().await?;
        if !response.status().is_success() {
            return Err(classify_error_response(response, &self.server_name).await);
        }

        let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();
        let endpoint_tx = Mutex::new(Some(endpoint_tx));
        let pending = self.pending.clone();
        let base_url = self.url.clone();
        let stream = response.bytes_stream().eventsource();
        let handle = tokio::spawn(async move {
            futures::pin_mut!(stream);
            while let Some(item) = stream.next().await {
                let event = match item {
                    Ok(event) => event,
                    Err(_) => break,
                };
                if event.event == "endpoint" {
                    if let Some(tx) = endpoint_tx.lock().await.take() {
                        let _ = tx.send(resolve_endpoint(&base_url, &event.data));
                    }
                } else if let Ok(msg) = serde_json::from_str::<Value>(&event.data) {
                    dispatch_jsonrpc(&pending, msg).await;
                }
            }
            fail_all_pending(&pending).await;
        });
        *self.reader.lock().await = Some(handle);

        let post_url = endpoint_rx
            .await
            .map_err(|_| McpError::Transport("sse server sent no endpoint".to_string()))?;
        *self.post_url.lock().await = Some(post_url);

        self.initialize().await
    }

    async fn close(&self) -> Result<(), McpError> {
        if let Some(handle) = self.reader.lock().await.take() {
            handle.abort();
        }
        *self.post_url.lock().await = None;
        fail_all_pending(&self.pending).await;
        Ok(())
    }
}

/// Build a client from a server config.
pub fn build_client(cfg: &crate::transport::McpServerConfig) -> Box<dyn McpClient> {
    let options = ClientOptions {
        pinned_protocol_version: cfg.protocol_version.clone(),
        tool_timeout: Duration::from_millis(cfg.tool_timeout_ms),
    };
    match &cfg.transport {
        McpTransport::Stdio { command, args, env } => Box::new(StdioMcpClient::new(
            cfg.name.clone(),
            command.clone(),
            args.clone(),
            env.clone(),
            options,
        )),
        McpTransport::Http { url, headers } => Box::new(HttpMcpClient::new(
            cfg.name.clone(),
            url.clone(),
            headers.clone(),
            options,
        )),
        McpTransport::Sse { url, headers } => Box::new(SseMcpClient::new(
            cfg.name.clone(),
            url.clone(),
            headers.clone(),
            options,
        )),
        McpTransport::Ws { url, headers } => Box::new(WsMcpClient::new(
            cfg.name.clone(),
            url.clone(),
            headers.clone(),
            options,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::accept_async;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn http_client_lists_tools() {
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
                        { "name": "read_file", "description": "read a file", "inputSchema": {"type": "object"} }
                    ]
                }
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    #[tokio::test]
    async fn http_client_calls_tool() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/call" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "content": "hello", "isError": false }
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        let result = client
            .call_tool("read_file", serde_json::json!({}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, serde_json::json!("hello"));
    }

    #[tokio::test]
    async fn http_client_surfaces_rpc_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32601, "message": "method not found" }
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        let err = client.connect().await.unwrap_err();
        assert!(matches!(err, McpError::Rpc { code: -32601, .. }));
    }

    #[tokio::test]
    async fn http_client_sends_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(header("Authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "tools": [] }
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token".to_string());
        let client = HttpMcpClient::new("fs", url, headers, ClientOptions::default());
        let tools = client.list_tools().await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn http_client_detects_oauth_step_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("WWW-Authenticate", "Bearer realm=\"mcp\""),
            )
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        let err = client.connect().await.unwrap_err();
        match err {
            McpError::Transport(msg) => {
                assert!(msg.contains("oauth step-up required"), "got: {msg}")
            }
            other => panic!("expected oauth transport error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_client_reconnects_on_session_expired() {
        let server = MockServer::start().await;
        // First tools/call reports an expired session.
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/call" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32001, "message": "session expired" }
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Reconnect initialize succeeds.
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "initialize" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": { "protocolVersion": "2024-11-05", "capabilities": {} }
            })))
            .mount(&server)
            .await;
        // Retried tools/call succeeds.
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/call" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": { "content": "ok", "isError": false }
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        let result = client
            .call_tool_resilient("read_file", serde_json::json!({}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, serde_json::json!("ok"));
    }

    #[tokio::test]
    async fn ws_client_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(item) = ws.next().await {
                let text = match item {
                    Ok(Message::Text(t)) => t,
                    _ => continue,
                };
                let msg: Value = serde_json::from_str(&text).unwrap();
                let rpc_method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                if rpc_method.starts_with("notifications/") {
                    continue;
                }
                let id = msg.get("id").cloned();
                let result = match rpc_method {
                    "initialize" => {
                        serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {}})
                    }
                    "tools/list" => serde_json::json!({"tools": [
                        {"name": "echo", "description": "echo tool", "inputSchema": {"type": "object"}}
                    ]}),
                    "tools/call" => serde_json::json!({"content": "pong", "isError": false}),
                    _ => serde_json::json!({}),
                };
                let resp = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                ws.send(Message::Text(resp.to_string().into()))
                    .await
                    .unwrap();
            }
        });

        let url = format!("ws://{addr}/mcp");
        let client = WsMcpClient::new("echo", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, serde_json::json!("pong"));
        client.close().await.unwrap();
        server.abort();
    }

    async fn read_http_request(stream: &mut TcpStream) -> (String, String, Vec<u8>) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut content_length = 0usize;
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                if key.trim().eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).await.unwrap();
        }
        (method, path, body)
    }

    async fn handle_sse_conn(
        mut stream: TcpStream,
        tx: tokio::sync::mpsc::Sender<String>,
        rx_slot: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<String>>>>,
    ) {
        let (method, path, body) = read_http_request(&mut stream).await;
        if method == "GET" && path.starts_with("/sse") {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .await;
            let _ = stream
                .write_all(b"event: endpoint\ndata: /messages\n\n")
                .await;
            let _ = stream.flush().await;
            let mut rx = match rx_slot.lock().await.take() {
                Some(rx) => rx,
                None => return,
            };
            while let Some(data) = rx.recv().await {
                let event = format!("event: message\ndata: {data}\n\n");
                if stream.write_all(event.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stream.flush().await;
            }
        } else if method == "POST" {
            let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let rpc_method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if !rpc_method.starts_with("notifications/") {
                let id = msg.get("id").cloned();
                let result = match rpc_method {
                    "initialize" => {
                        serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {}})
                    }
                    "tools/list" => serde_json::json!({"tools": [
                        {"name": "echo", "description": "echo tool", "inputSchema": {"type": "object"}}
                    ]}),
                    "tools/call" => serde_json::json!({"content": "pong", "isError": false}),
                    _ => serde_json::json!({}),
                };
                let resp = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                let _ = tx.send(resp.to_string()).await;
            }
            let _ = stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = stream.flush().await;
        } else {
            let _ = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await;
            let _ = stream.flush().await;
        }
    }

    #[tokio::test]
    async fn sse_client_round_trip() {
        use tokio::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (msg_tx, msg_rx) = mpsc::channel::<String>(32);
        let rx_slot: Arc<Mutex<Option<mpsc::Receiver<String>>>> =
            Arc::new(Mutex::new(Some(msg_rx)));
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = msg_tx.clone();
                let rx_slot = rx_slot.clone();
                tokio::spawn(async move {
                    handle_sse_conn(stream, tx, rx_slot).await;
                });
            }
        });

        let url = format!("http://{addr}/sse");
        let client = SseMcpClient::new("echo", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, serde_json::json!("pong"));
        client.close().await.unwrap();
        server.abort();
    }

    /// POSIX sh fixture speaking JSON-RPC over stdio: reads request lines,
    /// echoes the received id back with a canned result, and ignores client
    /// notifications. Before the `initialize`/`tools/list` responses it emits
    /// a server notification (no id) to exercise the client's id-matching
    /// loop. Responses are printed in subshells so the child's stdio buffers
    /// are flushed immediately.
    const STDIO_FIXTURE: &str = r#"
while IFS= read -r line; do
    case $line in
        *notifications/*) continue ;;
    esac
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    [ -z "$id" ] && continue
    method=$(printf '%s' "$line" | sed -n 's/.*"method":[[:space:]]*"\([^"]*\)".*/\1/p')
    case $method in
        initialize)
            (printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/progress"}')
            (printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{}}}")
            ;;
        tools/list)
            (printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/progress"}')
            (printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"echo tool\",\"inputSchema\":{\"type\":\"object\"}}]}}")
            ;;
        tools/call)
            (printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":\"pong\",\"isError\":false}}")
            ;;
    esac
done
"#;

    /// Fixture that answers `initialize` and then exits on the next request,
    /// so the client's pending `tools/call` sees EOF on stdout.
    const STDIO_EARLY_EXIT_FIXTURE: &str = r#"
while IFS= read -r line; do
    case $line in
        *notifications/*) continue ;;
    esac
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    [ -z "$id" ] && continue
    method=$(printf '%s' "$line" | sed -n 's/.*"method":[[:space:]]*"\([^"]*\)".*/\1/p')
    if [ "$method" = "initialize" ]; then
        (printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{}}}")
    else
        exit 0
    fi
done
"#;

    #[tokio::test]
    async fn stdio_client_round_trip() {
        let client = StdioMcpClient::new(
            "echo",
            "sh",
            vec!["-c".to_string(), STDIO_FIXTURE.to_string()],
            HashMap::new(),
            ClientOptions::default(),
        );
        // The timeout only guards against a broken fixture hanging the test.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            client.connect().await.unwrap();
            let tools = client.list_tools().await.unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "echo");
            let result = client
                .call_tool("echo", serde_json::json!({}))
                .await
                .unwrap();
            assert!(!result.is_error);
            assert_eq!(result.content, serde_json::json!("pong"));
            client.close().await.unwrap();
        })
        .await;
        assert!(outcome.is_ok(), "stdio round trip timed out");
    }

    #[tokio::test]
    async fn stdio_eof_returns_transport_error() {
        let client = StdioMcpClient::new(
            "flaky",
            "sh",
            vec!["-c".to_string(), STDIO_EARLY_EXIT_FIXTURE.to_string()],
            HashMap::new(),
            ClientOptions::default(),
        );
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            client.connect().await.unwrap();
            let err = client
                .call_tool("echo", serde_json::json!({}))
                .await
                .unwrap_err();
            assert!(
                matches!(err, McpError::Transport(_)),
                "expected transport error, got {err:?}"
            );
            client.close().await.unwrap();
        })
        .await;
        assert!(outcome.is_ok(), "stdio eof test timed out");
    }

    #[tokio::test]
    async fn stdio_call_before_connect_fails() {
        let client = StdioMcpClient::new(
            "echo",
            "sh",
            vec!["-c".to_string(), "exit 0".to_string()],
            HashMap::new(),
            ClientOptions::default(),
        );
        let err = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .unwrap_err();
        match err {
            McpError::Protocol(msg) => {
                assert!(msg.contains("client is not connected"), "got: {msg}")
            }
            other => panic!("expected protocol error, got {other:?}"),
        }
        let err = client.list_tools().await.unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)));
    }

    #[tokio::test]
    async fn ws_pending_request_fails_when_server_disconnects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(item) = ws.next().await {
                let text = match item {
                    Ok(Message::Text(t)) => t,
                    _ => continue,
                };
                let msg: Value = serde_json::from_str(&text).unwrap();
                let rpc_method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                match rpc_method {
                    "initialize" => {
                        let id = msg.get("id").cloned();
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
                        });
                        ws.send(Message::Text(resp.to_string().into()))
                            .await
                            .unwrap();
                    }
                    m if m.starts_with("notifications/") => continue,
                    // Close the socket with the request still pending.
                    _ => {
                        let _ = ws.close(None).await;
                        return;
                    }
                }
            }
        });

        let url = format!("ws://{addr}/mcp");
        let client = WsMcpClient::new("flaky", url, HashMap::new(), ClientOptions::default());
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            client.connect().await.unwrap();
            client.call_tool("echo", serde_json::json!({})).await
        })
        .await
        .expect("call_tool hung after server disconnect");
        assert!(
            matches!(outcome, Err(McpError::Transport(_))),
            "expected transport error, got {outcome:?}"
        );
        client.close().await.unwrap();
        server.abort();
    }

    /// Drive `classify_error_response` through `HttpMcpClient::connect` with a
    /// canned non-2xx response.
    async fn connect_error(
        status: u16,
        headers: Vec<(&str, &str)>,
        body: Option<Value>,
    ) -> McpError {
        let server = MockServer::start().await;
        let mut template = ResponseTemplate::new(status);
        for (key, value) in headers {
            template = template.insert_header(key, value);
        }
        template = match body {
            Some(json) => template.set_body_json(json),
            None => template.set_body_string("plain failure"),
        };
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(template)
            .mount(&server)
            .await;
        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap_err()
    }

    #[tokio::test]
    async fn http_client_classifies_error_responses() {
        // 404 carrying a JSON-RPC -32001 body is the session-expiry path.
        let err = connect_error(
            404,
            Vec::new(),
            Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32001, "message": "session expired" }
            })),
        )
        .await;
        assert!(
            matches!(err, McpError::Rpc { code: -32001, .. }),
            "expected rpc session-expiry error, got {err:?}"
        );

        // 500 with a plain body is a bare transport error.
        let err = connect_error(500, Vec::new(), None).await;
        match err {
            McpError::Transport(msg) => {
                assert!(msg.contains("server returned status 500"), "got: {msg}")
            }
            other => panic!("expected transport error, got {other:?}"),
        }

        // 401 without WWW-Authenticate is NOT classified as OAuth step-up.
        let err = connect_error(401, Vec::new(), None).await;
        match err {
            McpError::Transport(msg) => {
                assert!(
                    !msg.contains("oauth"),
                    "401 without WWW-Authenticate must not be oauth step-up: {msg}"
                );
                assert!(msg.contains("server returned status 401"), "got: {msg}");
            }
            other => panic!("expected transport error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_endpoint_absolute_passthrough_and_relative_join() {
        // Absolute endpoint payloads pass through unchanged.
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:1234/sse", "http://other:9/messages"),
            "http://other:9/messages"
        );
        // Relative paths join against the SSE base URL.
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:1234/sse", "/messages"),
            "http://127.0.0.1:1234/messages"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:1234/a/sse", "messages"),
            "http://127.0.0.1:1234/a/messages"
        );
        // An unparseable base falls back to the raw payload.
        assert_eq!(resolve_endpoint("not a url", "/messages"), "/messages");
    }

    /// Stub client whose `call_tool` always reports an expired session.
    struct AlwaysExpiredClient {
        connect_count: AtomicU64,
    }

    #[async_trait]
    impl McpClient for AlwaysExpiredClient {
        fn server_name(&self) -> &str {
            "stub"
        }

        async fn connect(&self) -> Result<(), McpError> {
            self.connect_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
            Ok(Vec::new())
        }

        async fn call_tool(&self, _name: &str, _args: Value) -> Result<McpToolResult, McpError> {
            Err(McpError::Rpc {
                code: -32001,
                message: "session expired".to_string(),
            })
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn call_tool_resilient_retries_only_once() {
        let client = AlwaysExpiredClient {
            connect_count: AtomicU64::new(0),
        };
        let err = client
            .call_tool_resilient("echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Rpc { code: -32001, .. }),
            "expected final rpc error, got {err:?}"
        );
        assert_eq!(client.connect_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn build_client_returns_correct_transport() {
        let cfg = crate::transport::McpServerConfig {
            name: "fs".to_string(),
            transport: McpTransport::Stdio {
                command: "echo".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
            },
            enabled: true,
            auto_approve: Vec::new(),
            connect_timeout_ms: 1000,
            protocol_version: None,
            tool_timeout_ms: 60_000,
        };
        let client = build_client(&cfg);
        assert_eq!(client.server_name(), "fs");
    }

    /// Bodies of the requests a wiremock server received, parsed as JSON.
    async fn received_bodies(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|req| serde_json::from_slice(&req.body).unwrap_or(Value::Null))
            .collect()
    }

    /// Mount an `initialize` mock keyed on the advertised protocol version.
    async fn mock_initialize(server: &MockServer, advertised: &str, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(serde_json::json!({
                "method": "initialize",
                "params": { "protocolVersion": advertised }
            })))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn rpc_error(code: i64, message: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": code, "message": message }
        }))
    }

    fn rpc_result(result: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": result
        }))
    }

    #[tokio::test]
    async fn http_negotiates_older_version_when_latest_is_rejected() {
        let server = MockServer::start().await;
        for rejected in ["2026-07-28", "2025-11-25"] {
            mock_initialize(
                &server,
                rejected,
                rpc_error(-32602, "unsupported protocol version"),
            )
            .await;
        }
        mock_initialize(
            &server,
            "2025-06-18",
            rpc_result(serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .and(header("MCP-Protocol-Version", "2025-06-18"))
            .respond_with(rpc_result(serde_json::json!({ "tools": [] })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        assert_eq!(client.protocol_version(), "2025-06-18");
        assert_eq!(
            client.server_capabilities(),
            serde_json::json!({ "tools": {} })
        );
        // The header matcher on the tools/list mock enforces that
        // post-initialize requests carry the negotiated version.
        let tools = client.list_tools().await.unwrap();
        assert!(tools.is_empty());

        let bodies = received_bodies(&server).await;
        let attempts: Vec<&str> = bodies
            .iter()
            .filter(|b| b.get("method").and_then(Value::as_str) == Some("initialize"))
            .filter_map(|b| {
                b.get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert_eq!(attempts, vec!["2026-07-28", "2025-11-25", "2025-06-18"]);
    }

    #[tokio::test]
    async fn http_adopts_server_returned_older_version() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": true } }
            })),
        )
        .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        // The server-returned version is adopted even though it is older than
        // every version we advertise.
        assert_eq!(client.protocol_version(), "2024-11-05");
        assert_eq!(
            client.server_capabilities(),
            serde_json::json!({ "tools": { "listChanged": true } })
        );
        // Stateful version: the initialized notification was sent.
        let bodies = received_bodies(&server).await;
        assert!(
            bodies
                .iter()
                .any(|b| b.get("method").and_then(Value::as_str)
                    == Some("notifications/initialized")),
            "expected notifications/initialized for a stateful version"
        );
    }

    #[tokio::test]
    async fn http_stateless_mode_sets_headers_and_skips_initialized() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2026-07-28",
                "capabilities": { "tools": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(header("MCP-Protocol-Version", "2026-07-28"))
            .and(header("Mcp-Method", "tools/call"))
            .and(header("Mcp-Name", "read_file"))
            .and(body_partial_json(serde_json::json!({
                "method": "tools/call",
                "params": {
                    "name": "read_file",
                    "_meta": {
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "legion",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })))
            .respond_with(rpc_result(
                serde_json::json!({ "content": "ok", "isError": false }),
            ))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        assert_eq!(client.protocol_version(), "2026-07-28");
        let result = client
            .call_tool("read_file", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.content, serde_json::json!("ok"));

        // Stateless mode never sends notifications/initialized, and the
        // initialize attempt itself already carried the routing headers.
        let requests = server.received_requests().await.unwrap_or_default();
        for req in &requests {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            assert_ne!(
                body.get("method").and_then(Value::as_str),
                Some("notifications/initialized"),
                "stateless mode must not send notifications/initialized"
            );
        }
        let initialize = requests
            .iter()
            .find(|req| {
                serde_json::from_slice::<Value>(&req.body)
                    .ok()
                    .and_then(|b| b.get("method").and_then(Value::as_str).map(str::to_string))
                    .as_deref()
                    == Some("initialize")
            })
            .expect("initialize request recorded");
        assert_eq!(
            initialize
                .headers
                .get("mcp-method")
                .and_then(|v| v.to_str().ok()),
            Some("initialize")
        );
        assert_eq!(
            initialize
                .headers
                .get("mcp-protocol-version")
                .and_then(|v| v.to_str().ok()),
            Some("2026-07-28")
        );
    }

    #[tokio::test]
    async fn http_falls_back_to_server_discover_when_initialize_is_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "initialize" }),
            ))
            .respond_with(rpc_error(-32601, "method not found"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "server/discover" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "stateless-server", "version": "1.0" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(header("MCP-Protocol-Version", "2026-07-28"))
            .and(header("Mcp-Method", "tools/list"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({ "tools": [] })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        assert_eq!(client.protocol_version(), "2026-07-28");
        assert_eq!(
            client.server_capabilities(),
            serde_json::json!({ "tools": { "listChanged": true } })
        );
        let tools = client.list_tools().await.unwrap();
        assert!(tools.is_empty());

        // A single initialize attempt, then discover; no initialized
        // notification in stateless mode.
        let bodies = received_bodies(&server).await;
        let methods: Vec<&str> = bodies
            .iter()
            .filter_map(|b| b.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(methods, vec!["initialize", "server/discover", "tools/list"]);
    }

    #[tokio::test]
    async fn http_parses_sse_framed_responses() {
        let server = MockServer::start().await;
        let sse = |msg: Value| format!("event: message\ndata: {msg}\n\n");
        mock_initialize(
            &server,
            "2026-07-28",
            ResponseTemplate::new(200).set_body_raw(
                sse(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": { "protocolVersion": "2025-06-18", "capabilities": {} }
                })),
                "text/event-stream",
            ),
        )
        .await;
        // The tools/list stream first carries an unrelated server
        // notification, then the response matching the request id.
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(serde_json::json!({ "method": "tools/list" })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!(
                    "{}{}",
                    sse(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress"
                    })),
                    sse(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": {
                            "tools": [
                                { "name": "read_file", "description": "read a file", "inputSchema": {"type": "object"} }
                            ]
                        }
                    }))
                ),
                "text/event-stream",
            ))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        assert_eq!(client.protocol_version(), "2025-06-18");
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    #[tokio::test]
    async fn http_session_id_is_captured_and_resent_for_stateful_versions() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {}
            }))
            .insert_header("Mcp-Session-Id", "sess-1"),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .and(header("Mcp-Session-Id", "sess-1"))
            .and(header("MCP-Protocol-Version", "2025-06-18"))
            .respond_with(rpc_result(serde_json::json!({ "tools": [] })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        // The header matchers on the mock enforce the session id resend.
        let tools = client.list_tools().await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn http_session_id_is_not_resent_when_stateless() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {}
            }))
            .insert_header("Mcp-Session-Id", "sess-1"),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/call" }),
            ))
            .respond_with(rpc_result(
                serde_json::json!({ "content": "ok", "isError": false }),
            ))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        client
            .call_tool("read_file", serde_json::json!({}))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap_or_default();
        for req in &requests {
            assert!(
                req.headers.get("mcp-session-id").is_none(),
                "session ids are retired in the stateless core"
            );
        }
    }

    #[tokio::test]
    async fn http_tool_call_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(10))
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": { "content": "late", "isError": false }
                    })),
            )
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let options = ClientOptions {
            tool_timeout: std::time::Duration::from_millis(50),
            ..ClientOptions::default()
        };
        let client = HttpMcpClient::new("fs", url, HashMap::new(), options);
        let err = client
            .call_tool("read_file", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Timeout(_)),
            "expected timeout, got {err:?}"
        );
    }

    #[tokio::test]
    async fn http_protocol_version_pin_skips_fallback_chain() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2025-03-26",
            rpc_result(serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {}
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .and(header("MCP-Protocol-Version", "2025-03-26"))
            .respond_with(rpc_result(serde_json::json!({ "tools": [] })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let options = ClientOptions {
            pinned_protocol_version: Some("2025-03-26".to_string()),
            ..ClientOptions::default()
        };
        let client = HttpMcpClient::new("fs", url, HashMap::new(), options);
        client.connect().await.unwrap();
        assert_eq!(client.protocol_version(), "2025-03-26");
        let tools = client.list_tools().await.unwrap();
        assert!(tools.is_empty());

        // Exactly one initialize attempt, advertising the pinned version.
        let bodies = received_bodies(&server).await;
        let attempts: Vec<&str> = bodies
            .iter()
            .filter(|b| b.get("method").and_then(Value::as_str) == Some("initialize"))
            .filter_map(|b| {
                b.get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert_eq!(attempts, vec!["2025-03-26"]);
    }

    #[tokio::test]
    async fn http_list_tools_follows_next_cursor() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "tools": [
                    { "name": "alpha", "description": "a", "inputSchema": {"type": "object"} }
                ],
                "nextCursor": "page-2"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(serde_json::json!({
                "method": "tools/list",
                "params": { "cursor": "page-2" }
            })))
            .respond_with(rpc_result(serde_json::json!({
                "tools": [
                    { "name": "beta", "description": "b", "inputSchema": {"type": "object"} }
                ]
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);

        // The follow-up request carried the cursor in params.
        let bodies = received_bodies(&server).await;
        assert!(
            bodies
                .iter()
                .any(|b| b.pointer("/params/cursor").and_then(Value::as_str) == Some("page-2")),
            "expected a tools/list request with cursor page-2"
        );
    }

    #[tokio::test]
    async fn http_list_tools_pagination_stops_at_page_cap() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "tools": [
                    { "name": "loop", "description": "", "inputSchema": {"type": "object"} }
                ],
                "nextCursor": "more"
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 100, "page cap bounds the collection");
        let pages = received_bodies(&server)
            .await
            .iter()
            .filter(|b| b.get("method").and_then(Value::as_str) == Some("tools/list"))
            .count();
        assert_eq!(pages, 100);
    }

    #[tokio::test]
    async fn http_lists_resources_with_pagination() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "resources": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "resources/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "resources": [{ "uri": "file:///a.txt", "name": "a" }],
                "nextCursor": "c2"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(serde_json::json!({
                "method": "resources/list",
                "params": { "cursor": "c2" }
            })))
            .respond_with(rpc_result(serde_json::json!({
                "resources": [{ "uri": "file:///b.txt", "name": "b" }]
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let resources = client.list_resources().await.unwrap();
        let uris: Vec<&str> = resources
            .iter()
            .filter_map(|r| r.get("uri").and_then(Value::as_str))
            .collect();
        assert_eq!(uris, vec!["file:///a.txt", "file:///b.txt"]);
    }

    #[tokio::test]
    async fn http_reads_resource() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "resources": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(serde_json::json!({
                "method": "resources/read",
                "params": { "uri": "file:///a.txt" }
            })))
            .respond_with(rpc_result(serde_json::json!({
                "contents": [{ "uri": "file:///a.txt", "mimeType": "text/plain", "text": "hi" }]
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let contents = client.read_resource("file:///a.txt").await.unwrap();
        assert_eq!(
            contents.pointer("/contents/0/text"),
            Some(&serde_json::json!("hi"))
        );
    }

    #[tokio::test]
    async fn http_lists_and_gets_prompts() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "prompts": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "prompts/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "prompts": [{ "name": "greet", "description": "greeting" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(serde_json::json!({
                "method": "prompts/get",
                "params": { "name": "greet", "arguments": { "who": "world" } }
            })))
            .respond_with(rpc_result(serde_json::json!({
                "messages": [{ "role": "user", "content": { "type": "text", "text": "hello world" } }]
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let prompts = client.list_prompts().await.unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].get("name"), Some(&serde_json::json!("greet")));
        let prompt = client
            .get_prompt("greet", Some(serde_json::json!({ "who": "world" })))
            .await
            .unwrap();
        assert_eq!(
            prompt.pointer("/messages/0/content/text"),
            Some(&serde_json::json!("hello world"))
        );
    }

    #[tokio::test]
    async fn http_capability_absent_short_circuits_introspection() {
        let server = MockServer::start().await;
        // Non-empty capability map without resources/prompts is authoritative.
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} }
            })),
        )
        .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();

        let resources = client.list_resources().await.unwrap();
        assert!(resources.is_empty());
        let prompts = client.list_prompts().await.unwrap();
        assert!(prompts.is_empty());
        let err = client.read_resource("file:///a").await.unwrap_err();
        match err {
            McpError::Protocol(msg) => assert!(msg.contains("resources"), "got: {msg}"),
            other => panic!("expected protocol error, got {other:?}"),
        }
        let err = client.get_prompt("greet", None).await.unwrap_err();
        match err {
            McpError::Protocol(msg) => assert!(msg.contains("prompts"), "got: {msg}"),
            other => panic!("expected protocol error, got {other:?}"),
        }

        // Nothing but initialize (and its notification) hit the wire.
        let bodies = received_bodies(&server).await;
        for body in &bodies {
            let method = body.get("method").and_then(Value::as_str).unwrap_or("");
            assert!(
                !method.starts_with("resources/") && !method.starts_with("prompts/"),
                "capability-gated method {method} must not hit the wire"
            );
        }
    }

    #[tokio::test]
    async fn http_introspection_is_permissive_when_capabilities_empty() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {}
            })),
        )
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

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let resources = client.list_resources().await.unwrap();
        assert_eq!(resources.len(), 1);
    }

    #[tokio::test]
    async fn http_tool_list_carries_annotations_and_output_schema() {
        let server = MockServer::start().await;
        mock_initialize(
            &server,
            "2026-07-28",
            rpc_result(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} }
            })),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/list" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "tools": [
                    {
                        "name": "query",
                        "description": "run a query",
                        "inputSchema": {"type": "object"},
                        "annotations": { "readOnlyHint": true },
                        "outputSchema": {"type": "object", "properties": {"rows": {"type": "integer"}}}
                    },
                    { "name": "plain", "description": "", "inputSchema": {"type": "object"} }
                ]
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        client.connect().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[0].annotations,
            Some(serde_json::json!({ "readOnlyHint": true }))
        );
        assert!(tools[0].output_schema.is_some());
        assert_eq!(tools[1].annotations, None);
        assert_eq!(tools[1].output_schema, None);
    }

    #[tokio::test]
    async fn http_call_tool_parses_structured_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/call" }),
            ))
            .respond_with(rpc_result(serde_json::json!({
                "content": [{ "type": "text", "text": "query ok" }],
                "isError": false,
                "structuredContent": { "rows": 3 }
            })))
            .mount(&server)
            .await;

        let url = format!("{}/rpc", server.uri());
        let client = HttpMcpClient::new("fs", url, HashMap::new(), ClientOptions::default());
        let result = client
            .call_tool("query", serde_json::json!({}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({ "rows": 3 }))
        );
        // The raw content passthrough is unchanged.
        assert_eq!(
            result.content,
            serde_json::json!([{ "type": "text", "text": "query ok" }])
        );
    }

    #[test]
    fn inject_stateless_meta_merges_and_preserves() {
        // No-op for stateful versions.
        let params = serde_json::json!({ "name": "t" });
        assert_eq!(inject_stateless_meta(params.clone(), "2025-06-18"), params);

        // Merges into an existing `_meta` without clobbering caller keys.
        let merged = inject_stateless_meta(
            serde_json::json!({ "name": "t", "_meta": { "custom": 1 } }),
            "2026-07-28",
        );
        assert_eq!(merged["name"], "t");
        assert_eq!(merged["_meta"]["custom"], 1);
        assert_eq!(
            merged["_meta"]["io.modelcontextprotocol/clientInfo"],
            serde_json::json!({ "name": "legion", "version": env!("CARGO_PKG_VERSION") })
        );
        assert!(merged["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object());

        // Non-object params pass through untouched.
        assert_eq!(
            inject_stateless_meta(Value::Null, "2026-07-28"),
            Value::Null
        );
    }

    #[test]
    fn version_rejection_classification() {
        assert!(is_version_rejection(&McpError::Rpc {
            code: -32602,
            message: "unsupported protocol version".to_string(),
        }));
        assert!(!is_version_rejection(&McpError::Rpc {
            code: -32601,
            message: "method not found".to_string(),
        }));
        assert!(is_version_rejection(&McpError::Transport(
            "server returned status 400".to_string()
        )));
        assert!(!is_version_rejection(&McpError::Transport(
            "server returned status 500".to_string()
        )));
        assert!(!is_version_rejection(&McpError::Transport(
            "oauth step-up required".to_string()
        )));
        assert!(!is_version_rejection(&McpError::Timeout("x".to_string())));
    }
}
