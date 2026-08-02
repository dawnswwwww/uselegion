//! LSP tool: ask a language server for definition, references, diagnostics,
//! or formatting.
//!
//! The tool speaks JSON-RPC over stdio to a configurable command. Only the
//! four actions in the design doc are implemented; anything else is rejected.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;

use async_trait::async_trait;
use legion_core::jsonrpc;
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};

use crate::policy::Policy;
use crate::tools::resolve_tool_path;

/// Default LSP server command when none is supplied in the input.
const DEFAULT_SERVER: &str = "rust-analyzer";

/// Supported LSP actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspAction {
    Definition,
    References,
    Diagnostics,
    Format,
}

impl LspAction {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "definition" => Some(LspAction::Definition),
            "references" => Some(LspAction::References),
            "diagnostics" => Some(LspAction::Diagnostics),
            "format" => Some(LspAction::Format),
            _ => None,
        }
    }

    fn method(self) -> &'static str {
        match self {
            LspAction::Definition => "textDocument/definition",
            LspAction::References => "textDocument/references",
            LspAction::Diagnostics => "textDocument/diagnostic",
            LspAction::Format => "textDocument/formatting",
        }
    }
}

/// Errors specific to the LSP backend.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to spawn LSP server '{0}': {1}")]
    Spawn(String, #[source] std::io::Error),

    #[error("LSP server closed stdout")]
    ServerClosed,

    #[error("invalid JSON-RPC response: {0}")]
    InvalidResponse(String),

    #[error("JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<jsonrpc::RpcError> for LspError {
    fn from(err: jsonrpc::RpcError) -> Self {
        LspError::Rpc {
            code: err.code,
            message: err.message,
        }
    }
}

/// Abstract LSP backend. The default implementation spawns a subprocess; tests
/// and custom integrations can provide their own backend.
#[async_trait]
pub trait LspBackend: Send + Sync {
    async fn definition(&self, path: &Path, line: usize, col: usize) -> Result<String, LspError>;
    async fn references(&self, path: &Path, line: usize, col: usize) -> Result<String, LspError>;
    async fn diagnostics(&self, path: &Path) -> Result<String, LspError>;
    async fn format(&self, path: &Path) -> Result<String, LspError>;
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// State for a single stdio LSP server connection.
struct StdioLspState {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
}

/// Default LSP backend that spawns a subprocess and speaks JSON-RPC over stdio.
pub struct StdioLspBackend {
    server: String,
    args: Vec<String>,
    state: tokio::sync::Mutex<Option<StdioLspState>>,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}

impl StdioLspBackend {
    pub fn new(server: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            server: server.into(),
            args,
            state: tokio::sync::Mutex::new(None),
            child: tokio::sync::Mutex::new(None),
        }
    }

    async fn ensure_connected(&self) -> Result<(), LspError> {
        let mut guard = self.child.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        let mut cmd = Command::new(&self.server);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| LspError::Spawn(self.server.clone(), e))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Io(std::io::Error::other("child has no stdin")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Io(std::io::Error::other("child has no stdout")))?;

        *guard = Some(child);
        *self.state.lock().await = Some(StdioLspState {
            stdin,
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
        });

        self.send_request(
            "initialize",
            json!({"processId": std::process::id(), "rootUri": Value::Null, "capabilities": {}}),
        )
        .await?;
        self.send_notification("initialized", json!({})).await?;
        Ok(())
    }

    fn next_id(state: &StdioLspState) -> u64 {
        jsonrpc::next_id(&state.next_id)
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), LspError> {
        let mut guard = self.state.lock().await;
        let state = guard
            .as_mut()
            .ok_or_else(|| LspError::Io(std::io::Error::other("LSP client not connected")))?;
        let msg = jsonrpc::build_notification(method, params);
        let line =
            serde_json::to_string(&msg).map_err(|e| LspError::InvalidResponse(e.to_string()))?;
        state.stdin.write_all(line.as_bytes()).await?;
        state.stdin.write_all(b"\r\n").await?;
        state.stdin.flush().await?;
        Ok(())
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let mut guard = self.state.lock().await;
        let state = guard
            .as_mut()
            .ok_or_else(|| LspError::Io(std::io::Error::other("LSP client not connected")))?;
        let id = Self::next_id(state);
        let msg = jsonrpc::build_request(id, method, params);
        let line =
            serde_json::to_string(&msg).map_err(|e| LspError::InvalidResponse(e.to_string()))?;
        state.stdin.write_all(line.as_bytes()).await?;
        state.stdin.write_all(b"\r\n").await?;
        state.stdin.flush().await?;

        let mut buf = String::new();
        loop {
            buf.clear();
            let n = state.stdout.read_line(&mut buf).await?;
            if n == 0 {
                return Err(LspError::ServerClosed);
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(trimmed)
                .map_err(|e| LspError::InvalidResponse(e.to_string()))?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return jsonrpc::parse_result(&value).map_err(LspError::from);
            }
        }
    }

    async fn request_action(
        &self,
        action: LspAction,
        path: &Path,
        line: usize,
        col: usize,
    ) -> Result<Value, LspError> {
        self.ensure_connected().await?;
        let uri = file_uri(path);
        let params = match action {
            LspAction::Definition | LspAction::References => {
                let mut p = json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": col},
                });
                if action == LspAction::References {
                    p["context"] = json!({"includeDeclaration": true});
                }
                p
            }
            LspAction::Diagnostics => json!({"textDocument": {"uri": uri}}),
            LspAction::Format => json!({
                "textDocument": {"uri": uri},
                "options": {"tabSize": 4, "insertSpaces": true},
            }),
        };
        self.send_request(action.method(), params).await
    }
}

#[async_trait]
impl LspBackend for StdioLspBackend {
    async fn definition(&self, path: &Path, line: usize, col: usize) -> Result<String, LspError> {
        let result = self
            .request_action(LspAction::Definition, path, line, col)
            .await?;
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
    }

    async fn references(&self, path: &Path, line: usize, col: usize) -> Result<String, LspError> {
        let result = self
            .request_action(LspAction::References, path, line, col)
            .await?;
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
    }

    async fn diagnostics(&self, path: &Path) -> Result<String, LspError> {
        let result = self
            .request_action(LspAction::Diagnostics, path, 0, 0)
            .await?;
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
    }

    async fn format(&self, path: &Path) -> Result<String, LspError> {
        let result = self.request_action(LspAction::Format, path, 0, 0).await?;
        // textDocument/formatting returns an array of TextEdit objects.
        let edits = result.as_array().cloned().unwrap_or_default();
        if edits.is_empty() {
            return Ok("no formatting changes needed".to_string());
        }
        apply_text_edits(path, &edits).await?;
        Ok(format!(
            "formatted {} with {} edit(s)",
            path.display(),
            edits.len()
        ))
    }
}

/// Apply a list of LSP TextEdit objects to a file.
async fn apply_text_edits(path: &Path, edits: &[Value]) -> Result<(), LspError> {
    let content = tokio::fs::read_to_string(path).await?;
    let mut new_content = content.clone();
    // Apply edits in reverse order so earlier edits don't shift later ranges.
    let mut sorted: Vec<&Value> = edits.iter().collect();
    sorted.sort_by(|a, b| {
        let a_start = a["range"]["start"]["line"].as_u64().unwrap_or(0);
        let b_start = b["range"]["start"]["line"].as_u64().unwrap_or(0);
        b_start.cmp(&a_start)
    });
    for edit in sorted {
        let range = &edit["range"];
        let start_line = range["start"]["line"].as_u64().unwrap_or(0) as usize;
        let start_col = range["start"]["character"].as_u64().unwrap_or(0) as usize;
        let end_line = range["end"]["line"].as_u64().unwrap_or(0) as usize;
        let end_col = range["end"]["character"].as_u64().unwrap_or(0) as usize;
        let replacement = edit["newText"].as_str().unwrap_or("");
        new_content = apply_edit(
            &new_content,
            start_line,
            start_col,
            end_line,
            end_col,
            replacement,
        );
    }
    tokio::fs::write(path, new_content).await?;
    Ok(())
}

fn apply_edit(
    content: &str,
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
    replacement: &str,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx < start_line || idx > end_line {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if idx == start_line && idx == end_line {
            let prefix = line.chars().take(start_col).collect::<String>();
            let suffix = line.chars().skip(end_col).collect::<String>();
            out.push_str(&prefix);
            out.push_str(replacement);
            out.push_str(&suffix);
            out.push('\n');
        } else if idx == start_line {
            let prefix = line.chars().take(start_col).collect::<String>();
            out.push_str(&prefix);
            if idx == end_line - 1 {
                let suffix = line.chars().skip(end_col).collect::<String>();
                out.push_str(replacement);
                out.push_str(&suffix);
            }
            out.push('\n');
        } else if idx == end_line {
            let suffix = line.chars().skip(end_col).collect::<String>();
            out.push_str(replacement);
            out.push_str(&suffix);
            out.push('\n');
        } else {
            // Line inside the replaced range is dropped.
        }
    }
    // Preserve trailing newline exactly if the original had one.
    if !content.ends_with('\n') {
        out.pop();
    }
    out
}

/// LSP tool implementation.
pub struct LspTool {
    policy: Policy,
    default_server: String,
    default_args: Vec<String>,
    backend: Option<std::sync::Arc<dyn LspBackend>>,
}

impl LspTool {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            default_server: DEFAULT_SERVER.to_string(),
            default_args: Vec::new(),
            backend: None,
        }
    }

    pub fn with_server(mut self, server: impl Into<String>, args: Vec<String>) -> Self {
        self.default_server = server.into();
        self.default_args = args;
        self
    }

    /// Replace the default stdio backend with a custom one (used in tests).
    pub fn with_backend(mut self, backend: std::sync::Arc<dyn LspBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    fn build_backend(&self, input_server: Option<&str>) -> std::sync::Arc<dyn LspBackend> {
        if let Some(backend) = &self.backend {
            return backend.clone();
        }
        let server = input_server.unwrap_or(&self.default_server).to_string();
        std::sync::Arc::new(StdioLspBackend::new(server, self.default_args.clone()))
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Ask a language server for definition, references, diagnostics, or formatting."
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
                    "enum": ["definition", "references", "diagnostics", "format"],
                    "description": "LSP action to perform"
                },
                "path": { "type": "string", "description": "file path relative to workspace" },
                "line": { "type": "integer", "description": "0-based line number" },
                "column": { "type": "integer", "description": "0-based column number" },
                "server": { "type": "string", "description": "LSP server command (defaults to rust-analyzer)" }
            },
            "required": ["action", "path", "line", "column"]
        })
    }

    fn is_read_only(&self, input: &Value) -> bool {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        action != "format"
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Lsp
    }

    fn namespace(&self) -> ToolNamespace {
        ToolNamespace::Legion
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let action_str = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'action' parameter".to_string()))?;
        let action = LspAction::from_str(action_str).ok_or_else(|| {
            ToolError::InvalidParams(format!("unknown lsp action '{action_str}'"))
        })?;
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'path' parameter".to_string()))?;
        let line = input
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::InvalidParams("missing 'line' parameter".to_string()))?
            as usize;
        let col = input
            .get("column")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::InvalidParams("missing 'column' parameter".to_string()))?
            as usize;
        let server = input.get("server").and_then(Value::as_str);

        let resolved = resolve_tool_path(&ctx, path, self.policy.workspace_only)?;
        let backend = self.build_backend(server);

        let result = match action {
            LspAction::Definition => backend.definition(&resolved, line, col).await,
            LspAction::References => backend.references(&resolved, line, col).await,
            LspAction::Diagnostics => backend.diagnostics(&resolved).await,
            LspAction::Format => backend.format(&resolved).await,
        }
        .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolResult::ok(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Approval;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn policy() -> Policy {
        Policy {
            approval: Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

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
            plan_mode_tracker: None,
        }
    }

    struct RecordingBackend {
        calls: std::sync::Mutex<Vec<(String, PathBuf, usize, usize)>>,
    }

    #[async_trait]
    impl LspBackend for RecordingBackend {
        async fn definition(
            &self,
            path: &Path,
            line: usize,
            col: usize,
        ) -> Result<String, LspError> {
            self.calls.lock().unwrap().push((
                "definition".to_string(),
                path.to_path_buf(),
                line,
                col,
            ));
            Ok(r#"{"uri":"file:///x.rs","range":{"start":{"line":0,"character":0}}}"#.to_string())
        }
        async fn references(
            &self,
            path: &Path,
            line: usize,
            col: usize,
        ) -> Result<String, LspError> {
            self.calls.lock().unwrap().push((
                "references".to_string(),
                path.to_path_buf(),
                line,
                col,
            ));
            Ok("[]".to_string())
        }
        async fn diagnostics(&self, path: &Path) -> Result<String, LspError> {
            self.calls
                .lock()
                .unwrap()
                .push(("diagnostics".to_string(), path.to_path_buf(), 0, 0));
            Ok("{}".to_string())
        }
        async fn format(&self, path: &Path) -> Result<String, LspError> {
            self.calls
                .lock()
                .unwrap()
                .push(("format".to_string(), path.to_path_buf(), 0, 0));
            Ok("formatted".to_string())
        }
    }

    #[test]
    fn schema_requires_action_path_line_column() {
        let tool = LspTool::new(policy());
        let schema = tool.schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("action")));
        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("line")));
        assert!(required.contains(&json!("column")));
    }

    #[test]
    fn format_is_not_read_only() {
        let tool = LspTool::new(policy());
        assert!(tool.is_read_only(&json!({"action": "definition", "path": "x"})));
        assert!(tool.is_read_only(&json!({"action": "references", "path": "x"})));
        assert!(tool.is_read_only(&json!({"action": "diagnostics", "path": "x"})));
        assert!(!tool.is_read_only(&json!({"action": "format", "path": "x"})));
    }

    #[tokio::test]
    async fn tool_routes_to_backend_and_returns_result() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("x.rs"), "fn main() {}")
            .await
            .unwrap();
        let backend = std::sync::Arc::new(RecordingBackend {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let tool = LspTool::new(policy()).with_backend(backend.clone());
        let res = tool
            .execute(
                json!({"action": "definition", "path": "x.rs", "line": 0, "column": 3}),
                ctx(&dir),
            )
            .await
            .unwrap();
        assert!(!res.is_error);
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "definition");
        assert_eq!(calls[0].2, 0);
        assert_eq!(calls[0].3, 3);
    }

    #[test]
    fn apply_edit_replaces_single_line() {
        // Insert "bar" right after "foo" (column 6) to produce "foobar".
        let result = apply_edit("fn foo() {}\n", 0, 6, 0, 6, "bar");
        assert_eq!(result, "fn foobar() {}\n");
    }

    #[test]
    fn apply_edit_preserves_no_trailing_newline() {
        let result = apply_edit("fn foo() {}", 0, 6, 0, 6, "bar");
        assert_eq!(result, "fn foobar() {}");
    }

    #[tokio::test]
    async fn format_action_applies_edits() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.rs");
        tokio::fs::write(&path, "fn  main() {}\n").await.unwrap();

        // Replace one of the two spaces between "fn" and "main".
        apply_text_edits(
            &path,
            &[json!({
                "range": {
                    "start": {"line": 0, "character": 2},
                    "end": {"line": 0, "character": 4}
                },
                "newText": " "
            })],
        )
        .await
        .unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "fn main() {}\n");
    }
}
