//! MCP client trait plus stdio, http, sse and websocket transports.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::{Sink, SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

use crate::transport::McpTransport;

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
}

/// Result returned by an MCP `tools/call`.
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: Value,
    pub is_error: bool,
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

/// Client talking to a single MCP server.
#[async_trait]
pub trait McpClient: Send + Sync {
    fn server_name(&self) -> &str;
    async fn connect(&self) -> Result<(), McpError>;
    async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError>;
    async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError>;
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
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    pending.lock().await.insert(id, tx);
    (id, rx)
}

/// Parse a JSON-RPC message into its `result` or an `Rpc` error.
fn parse_jsonrpc(msg: &Value) -> Result<Value, McpError> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string();
        return Err(McpError::Rpc { code, message });
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
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

/// Decode a `tools/list` result into tool descriptors.
fn parse_tool_list(result: Value) -> Result<Vec<McpToolDesc>, McpError> {
    let tools = result
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
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
        descs.push(McpToolDesc {
            name,
            description,
            input_schema,
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
    McpToolResult { content, is_error }
}

/// Standard `initialize` params shared by every transport.
fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": { "name": "legion", "version": "0.1.0" }
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
        if let Some(err) = msg.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return McpError::Rpc { code, message };
        }
    }
    McpError::Transport(format!("server returned status {status}"))
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
}

impl StdioMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            command: command.into(),
            args,
            env,
            next_id: AtomicU64::new(1),
            state: Mutex::new(None),
            child: Mutex::new(None),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

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

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), McpError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
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
}

#[async_trait]
impl McpClient for StdioMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
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

        self.send_request("initialize", initialize_params()).await?;
        self.send_notification("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
        let result = self
            .send_request("tools/list", serde_json::json!({}))
            .await?;
        parse_tool_list(result)
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError> {
        let result = self
            .send_request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
            )
            .await?;
        Ok(parse_tool_result(result))
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
pub struct HttpMcpClient {
    server_name: String,
    url: String,
    headers: HashMap<String, String>,
    next_id: AtomicU64,
    client: reqwest::Client,
}

impl HttpMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        url: impl Into<String>,
        headers: HashMap<String, String>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            url: url.into(),
            headers,
            next_id: AtomicU64::new(1),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn post(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut req = self.client.post(&self.url).json(&request);
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        let response = req.send().await?;
        if !response.status().is_success() {
            return Err(classify_error_response(response, &self.server_name).await);
        }
        let msg: Value = response.json().await?;
        parse_jsonrpc(&msg)
    }
}

#[async_trait]
impl McpClient for HttpMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    async fn connect(&self) -> Result<(), McpError> {
        self.post("initialize", initialize_params()).await?;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
        let result = self.post("tools/list", serde_json::json!({})).await?;
        parse_tool_list(result)
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError> {
        let result = self
            .post(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
            )
            .await?;
        Ok(parse_tool_result(result))
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
}

impl WsMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        url: impl Into<String>,
        headers: HashMap<String, String>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            url: url.into(),
            headers,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            write: Mutex::new(None),
            reader: Mutex::new(None),
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let (id, rx) = register_request(&self.pending, &self.next_id).await;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
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
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
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

        self.request("initialize", initialize_params()).await?;
        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        parse_tool_list(result)
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
            )
            .await?;
        Ok(parse_tool_result(result))
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
}

impl SseMcpClient {
    pub fn new(
        server_name: impl Into<String>,
        url: impl Into<String>,
        headers: HashMap<String, String>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            url: url.into(),
            headers,
            next_id: AtomicU64::new(1),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            post_url: Mutex::new(None),
            reader: Mutex::new(None),
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

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let (id, rx) = register_request(&self.pending, &self.next_id).await;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
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
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.post_endpoint(msg).await?;
        Ok(())
    }
}

#[async_trait]
impl McpClient for SseMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
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

        self.request("initialize", initialize_params()).await?;
        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        parse_tool_list(result)
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
            )
            .await?;
        Ok(parse_tool_result(result))
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
    match &cfg.transport {
        McpTransport::Stdio { command, args, env } => Box::new(StdioMcpClient::new(
            cfg.name.clone(),
            command.clone(),
            args.clone(),
            env.clone(),
        )),
        McpTransport::Http { url, headers } => Box::new(HttpMcpClient::new(
            cfg.name.clone(),
            url.clone(),
            headers.clone(),
        )),
        McpTransport::Sse { url, headers } => Box::new(SseMcpClient::new(
            cfg.name.clone(),
            url.clone(),
            headers.clone(),
        )),
        McpTransport::Ws { url, headers } => Box::new(WsMcpClient::new(
            cfg.name.clone(),
            url.clone(),
            headers.clone(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
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
        let client = HttpMcpClient::new("fs", url, HashMap::new());
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
        let client = HttpMcpClient::new("fs", url, HashMap::new());
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
        let client = HttpMcpClient::new("fs", url, HashMap::new());
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
        let client = HttpMcpClient::new("fs", url, headers);
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
        let client = HttpMcpClient::new("fs", url, HashMap::new());
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
        let client = HttpMcpClient::new("fs", url, HashMap::new());
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
        let client = WsMcpClient::new("echo", url, HashMap::new());
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
                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
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
        let client = SseMcpClient::new("echo", url, HashMap::new());
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
        let client = WsMcpClient::new("flaky", url, HashMap::new());
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
        let client = HttpMcpClient::new("fs", url, HashMap::new());
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
        };
        let client = build_client(&cfg);
        assert_eq!(client.server_name(), "fs");
    }
}
