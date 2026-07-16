//! `browser` tool (tools-p1p2 gap, Phase C, lightweight CDP backend).
//!
//! Drives a headless Chromium through the Chrome DevTools Protocol over a
//! WebSocket. This is the deliberately small read-only slice from gap doc
//! §4.4: `navigate` / `read` / `screenshot` only — no clicking, typing, or
//! scripting. The backend is configured entirely through the opaque
//! `ToolConfig.extra` (`tools.browser.cdpUrl`, `tools.browser.timeoutSeconds`),
//! so no config schema change is needed.
//!
//! Every invocation opens a one-shot CDP connection (no pooling): create a
//! fresh target, attach in flatten mode, run the command, then close the
//! target and the socket. Simple and reliable at the cost of per-call latency.
//!
//! Testability note: the message codec (`build_cdp_command` /
//! `parse_cdp_response`) is factored into pure functions with unit tests. The
//! WebSocket round-trip itself cannot be exercised in this environment (no
//! CDP server), so `cdp_call` is only covered indirectly through the
//! "backend not configured" path.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::{Sink, SinkExt, Stream, StreamExt};
use legion_runtime::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::policy::Policy;

/// Maximum characters returned by the `read` action.
const MAX_READ_CHARS: usize = 8000;

/// Grace period after `Page.navigate` before reading the DOM.
///
/// Proper load detection waits for the `Page.loadEventFired` event; this
/// lightweight slice simply settles for a fixed delay (documented trade-off
/// in the gap doc: keep Phase C small, real event waiting is a later slice).
const POST_NAVIGATE_SETTLE: Duration = Duration::from_millis(500);

/// Build a CDP command frame, optionally addressed to a flattened session.
pub fn build_cdp_command(id: u64, session_id: Option<&str>, method: &str, params: Value) -> Value {
    let mut command = json!({
        "id": id,
        "method": method,
        "params": params,
    });
    if let Some(session_id) = session_id {
        command["sessionId"] = Value::String(session_id.to_string());
    }
    command
}

/// Parse a CDP response frame into its `result` payload.
///
/// - `{"id":N,"result":...}` → `Ok(result)` (`result` may be absent → `Null`).
/// - `{"id":N,"error":{"message":...}}` → `Err(message)`.
/// - Event frames carry no `id` and are rejected here; the receive loop skips
///   them before ever calling this function.
pub fn parse_cdp_response(frame: &Value) -> Result<Value, String> {
    if frame.get("id").is_none() {
        return Err("CDP event frame has no id".to_string());
    }
    match frame.get("error") {
        Some(error) => {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown CDP error");
            Err(format!("CDP error: {message}"))
        }
        None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// Truncate to at most `max` characters (not bytes).
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

/// Extract the string value of a `Runtime.evaluate` result
/// (`{"result": {"result": {"value": ...}}}` with `returnByValue: true`).
fn eval_string(result: &Value) -> String {
    result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Send one CDP command and wait for the matching response.
///
/// Event frames (no `id`) and responses for other ids are skipped. The whole
/// round-trip is bounded by `timeout`.
async fn cdp_command<W, R>(
    sink: &mut W,
    stream: &mut R,
    next_id: &mut u64,
    session_id: Option<&str>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, ToolError>
where
    W: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    R: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let id = *next_id;
    *next_id += 1;

    let command = build_cdp_command(id, session_id, method, params);
    let text = serde_json::to_string(&command)
        .map_err(|e| ToolError::Execution(format!("failed to serialize CDP command: {e}")))?;
    sink.send(Message::text(text))
        .await
        .map_err(|e| ToolError::Execution(format!("CDP send failed for '{method}': {e}")))?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(ToolError::Execution(format!(
                "CDP command '{method}' timed out after {timeout:?}"
            )));
        }
        let frame = match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(Some(Ok(Message::Close(_)))) => {
                return Err(ToolError::Execution(format!(
                    "CDP connection closed while waiting for '{method}'"
                )));
            }
            // Ping/Pong/Binary frames are not part of the CDP conversation.
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(ToolError::Execution(format!("CDP receive failed: {e}")));
            }
            Ok(None) => {
                return Err(ToolError::Execution(
                    "CDP stream ended before response".to_string(),
                ));
            }
            Err(_) => {
                return Err(ToolError::Execution(format!(
                    "CDP command '{method}' timed out after {timeout:?}"
                )));
            }
        };
        let frame: Value = match serde_json::from_str(&frame) {
            Ok(value) => value,
            Err(_) => continue, // tolerate non-JSON frames
        };
        match frame.get("id").and_then(Value::as_u64) {
            Some(frame_id) if frame_id == id => {
                return parse_cdp_response(&frame).map_err(ToolError::Execution);
            }
            // Event frame or a response for another command: keep waiting.
            _ => continue,
        }
    }
}

/// `browser`: read-only web browsing over CDP (navigate / read / screenshot).
pub struct BrowserTool {
    pub policy: Policy,
    cdp_url: Option<String>,
    timeout: Duration,
}

impl BrowserTool {
    pub fn new(policy: Policy, cdp_url: Option<String>, timeout: Duration) -> Self {
        Self {
            policy,
            cdp_url,
            timeout,
        }
    }

    /// One-shot CDP round-trip: connect, create a target, run the action,
    /// close the target. Not unit-testable without a live CDP server.
    async fn cdp_call(
        &self,
        cdp_url: &str,
        action: &str,
        url: Option<&str>,
        selector: Option<&str>,
        workspace: &Path,
    ) -> Result<String, ToolError> {
        let (ws, _) = tokio::time::timeout(self.timeout, connect_async(cdp_url))
            .await
            .map_err(|_| {
                ToolError::Execution(format!(
                    "browser CDP connect to {cdp_url} timed out after {:?}",
                    self.timeout
                ))
            })?
            .map_err(|e| ToolError::Execution(format!("browser CDP connect failed: {e}")))?;
        let (mut sink, mut stream) = ws.split();

        let mut next_id: u64 = 1;

        // Create a fresh page target and attach in flatten mode so that
        // subsequent commands are addressed with the returned sessionId.
        let result = cdp_command(
            &mut sink,
            &mut stream,
            &mut next_id,
            None,
            "Target.createTarget",
            json!({ "url": url.unwrap_or("about:blank") }),
            self.timeout,
        )
        .await?;
        let target_id = result
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::Execution("CDP Target.createTarget returned no targetId".to_string())
            })?
            .to_string();

        let result = cdp_command(
            &mut sink,
            &mut stream,
            &mut next_id,
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            self.timeout,
        )
        .await?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::Execution("CDP Target.attachToTarget returned no sessionId".to_string())
            })?
            .to_string();

        let outcome = async {
            cdp_command(
                &mut sink,
                &mut stream,
                &mut next_id,
                Some(&session_id),
                "Page.enable",
                json!({}),
                self.timeout,
            )
            .await?;

            if let Some(url) = url {
                cdp_command(
                    &mut sink,
                    &mut stream,
                    &mut next_id,
                    Some(&session_id),
                    "Page.navigate",
                    json!({ "url": url }),
                    self.timeout,
                )
                .await?;
                // Simplified load detection: settle briefly instead of waiting
                // for Page.loadEventFired (see POST_NAVIGATE_SETTLE).
                tokio::time::sleep(POST_NAVIGATE_SETTLE).await;
            }

            match action {
                "navigate" => {
                    let result = cdp_command(
                        &mut sink,
                        &mut stream,
                        &mut next_id,
                        Some(&session_id),
                        "Runtime.evaluate",
                        json!({
                            "expression": "document.title + \" — \" + location.href",
                            "returnByValue": true,
                        }),
                        self.timeout,
                    )
                    .await?;
                    Ok(eval_string(&result))
                }
                "read" => {
                    let expression = match selector {
                        // serde_json::to_string JSON-escapes the selector for
                        // safe embedding into the JS expression.
                        Some(sel) => format!(
                            "document.querySelector({})?.innerText ?? \"\"",
                            serde_json::to_string(sel).unwrap_or_else(|_| "\"\"".to_string())
                        ),
                        None => "document.body.innerText".to_string(),
                    };
                    let result = cdp_command(
                        &mut sink,
                        &mut stream,
                        &mut next_id,
                        Some(&session_id),
                        "Runtime.evaluate",
                        json!({
                            "expression": expression,
                            "returnByValue": true,
                        }),
                        self.timeout,
                    )
                    .await?;
                    Ok(truncate_chars(&eval_string(&result), MAX_READ_CHARS))
                }
                "screenshot" => {
                    let result = cdp_command(
                        &mut sink,
                        &mut stream,
                        &mut next_id,
                        Some(&session_id),
                        "Page.captureScreenshot",
                        json!({ "format": "png" }),
                        self.timeout,
                    )
                    .await?;
                    let b64 = result.get("data").and_then(Value::as_str).ok_or_else(|| {
                        ToolError::Execution(
                            "CDP Page.captureScreenshot returned no data".to_string(),
                        )
                    })?;
                    let bytes = STANDARD.decode(b64).map_err(|e| {
                        ToolError::Execution(format!("invalid screenshot base64: {e}"))
                    })?;
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let dir = workspace.join("generated");
                    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
                        ToolError::Execution(format!("failed to create {}: {e}", dir.display()))
                    })?;
                    let path = dir.join(format!("screenshot-{timestamp}.png"));
                    tokio::fs::write(&path, &bytes).await.map_err(|e| {
                        ToolError::Execution(format!("failed to write {}: {e}", path.display()))
                    })?;
                    Ok(path.display().to_string())
                }
                other => Err(ToolError::InvalidParams(format!(
                    "unsupported browser action '{other}'"
                ))),
            }
        }
        .await;

        // Best-effort cleanup: closing the target / socket must never mask
        // the actual outcome, so errors are deliberately ignored.
        let _ = cdp_command(
            &mut sink,
            &mut stream,
            &mut next_id,
            None,
            "Target.closeTarget",
            json!({ "targetId": target_id }),
            Duration::from_secs(5),
        )
        .await;
        let _ = sink.close().await;

        outcome
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Drive a headless browser over CDP to navigate to a URL, read the \
         page's text content (optionally scoped by CSS selector), or take a \
         PNG screenshot (written under the workspace's generated/ directory). \
         Read-only: no clicking, typing, or form submission."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "read", "screenshot"],
                    "description": "Browser action to perform."
                },
                "url": {
                    "type": "string",
                    "description": "URL to open. Required for 'navigate'; when omitted the browser opens about:blank."
                },
                "selector": {
                    "type": "string",
                    "description": "Optional CSS selector; 'read' extracts innerText of the first matching element."
                }
            },
            "required": ["action"]
        })
    }

    fn is_read_only(&self, input: &Value) -> bool {
        // `screenshot` writes a file into the workspace, so it is not
        // read-only; navigate/read only observe the page.
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        matches!(action, "navigate" | "read")
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'action' parameter".to_string()))?;
        match action {
            "navigate" | "read" | "screenshot" => {}
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "unsupported browser action '{other}'"
                )));
            }
        }
        tracing::info!(tool = "browser", action, "browser tool invoked");

        let cdp_url = match &self.cdp_url {
            Some(url) => url.clone(),
            None => {
                return Ok(ToolResult::error(
                    "browser backend not configured (set tools.browser.cdpUrl)",
                ));
            }
        };

        let url = params.get("url").and_then(Value::as_str);
        if action == "navigate" && url.is_none() {
            return Err(ToolError::InvalidParams(
                "'navigate' requires a 'url' parameter".to_string(),
            ));
        }
        let selector = params.get("selector").and_then(Value::as_str);

        let output = self
            .cdp_call(&cdp_url, action, url, selector, &ctx.workspace)
            .await?;
        Ok(ToolResult::ok(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(dir: &TempDir) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
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
            background_tasks: None,
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
    fn build_cdp_command_without_session() {
        let command = build_cdp_command(7, None, "Page.enable", json!({}));
        assert_eq!(command["id"], 7);
        assert_eq!(command["method"], "Page.enable");
        assert_eq!(command["params"], json!({}));
        assert!(command.get("sessionId").is_none());
    }

    #[test]
    fn build_cdp_command_with_session_and_params() {
        let command = build_cdp_command(
            3,
            Some("sess-1"),
            "Page.navigate",
            json!({ "url": "https://example.com" }),
        );
        assert_eq!(command["id"], 3);
        assert_eq!(command["sessionId"], "sess-1");
        assert_eq!(command["params"]["url"], "https://example.com");
    }

    #[test]
    fn parse_cdp_response_extracts_result() {
        let frame = json!({"id": 1, "result": {"targetId": "abc"}});
        let result = parse_cdp_response(&frame).unwrap();
        assert_eq!(result["targetId"], "abc");
    }

    #[test]
    fn parse_cdp_response_missing_result_yields_null() {
        let frame = json!({"id": 2});
        assert_eq!(parse_cdp_response(&frame).unwrap(), Value::Null);
    }

    #[test]
    fn parse_cdp_response_surfaces_protocol_error() {
        let frame = json!({"id": 3, "error": {"code": -32601, "message": "Method not found"}});
        let err = parse_cdp_response(&frame).unwrap_err();
        assert!(err.contains("Method not found"), "unexpected: {err}");
    }

    #[test]
    fn parse_cdp_response_error_without_message_falls_back() {
        let frame = json!({"id": 4, "error": {"code": -1}});
        let err = parse_cdp_response(&frame).unwrap_err();
        assert!(err.contains("unknown CDP error"), "unexpected: {err}");
    }

    #[test]
    fn parse_cdp_response_rejects_event_frame() {
        let frame = json!({"method": "Page.loadEventFired", "params": {}});
        let err = parse_cdp_response(&frame).unwrap_err();
        assert!(err.contains("no id"), "unexpected: {err}");
    }

    #[test]
    fn truncate_chars_respects_limit() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello world", 5), "hello");
        // Multi-byte characters must not panic on byte boundaries.
        let text: String = "中".repeat(9000);
        assert_eq!(truncate_chars(&text, MAX_READ_CHARS).chars().count(), 8000);
    }

    #[test]
    fn read_only_depends_on_action() {
        let tool = BrowserTool::new(open_policy(), None, Duration::from_secs(30));
        assert!(tool.is_read_only(&json!({"action": "navigate"})));
        assert!(tool.is_read_only(&json!({"action": "read"})));
        assert!(!tool.is_read_only(&json!({"action": "screenshot"})));
        assert!(!tool.is_read_only(&json!({})));
        assert!(tool.is_concurrency_safe(&json!({})));
    }

    #[test]
    fn schema_requires_action() {
        let tool = BrowserTool::new(open_policy(), None, Duration::from_secs(30));
        let schema = tool.schema();
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["navigate", "read", "screenshot"])
        );
    }

    #[tokio::test]
    async fn execute_without_cdp_url_returns_error_result() {
        let dir = TempDir::new().unwrap();
        let tool = BrowserTool::new(open_policy(), None, Duration::from_secs(30));
        let result = tool
            .execute(json!({"action": "read"}), ctx(&dir))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("browser backend not configured"));
    }

    #[tokio::test]
    async fn execute_rejects_unsupported_action() {
        let dir = TempDir::new().unwrap();
        let tool = BrowserTool::new(
            open_policy(),
            Some("ws://127.0.0.1:9222".to_string()),
            Duration::from_secs(30),
        );
        let err = tool
            .execute(json!({"action": "click"}), ctx(&dir))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unsupported browser action"));
    }

    #[tokio::test]
    async fn execute_navigate_requires_url() {
        let dir = TempDir::new().unwrap();
        let tool = BrowserTool::new(
            open_policy(),
            Some("ws://127.0.0.1:9222".to_string()),
            Duration::from_secs(30),
        );
        let err = tool
            .execute(json!({"action": "navigate"}), ctx(&dir))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a 'url'"));
    }
}
