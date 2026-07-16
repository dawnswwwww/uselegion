use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use legion_runtime::{
    CoordinatorPlan, SubagentKind, SubagentRequest, SubagentStatus, Tool, ToolContext, ToolError,
    ToolResult, run_coordinator_plan,
};
use serde_json::json;

use crate::policy::{Approval, Policy};
use crate::sandbox::{ExecResult, LocalSandboxBackend, SandboxBackend};

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve a user-provided path relative to the workspace and optionally enforce
/// that it stays inside the workspace.
pub fn resolve_tool_path(
    ctx: &ToolContext,
    path: &str,
    workspace_only: bool,
) -> Result<PathBuf, ToolError> {
    if workspace_only {
        // Reject paths that attempt to escape the workspace via parent components.
        for component in Path::new(path).components() {
            if matches!(component, std::path::Component::ParentDir) {
                return Err(ToolError::Execution(
                    "paths containing '..' are not allowed".to_string(),
                ));
            }
        }
    }

    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        ctx.workspace.join(path)
    };

    if workspace_only {
        let canonical_ws = ctx
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| ctx.workspace.clone());
        let canonical_path = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());

        if !canonical_path.starts_with(&canonical_ws) {
            return Err(ToolError::Execution(
                "absolute paths outside the workspace are not allowed".to_string(),
            ));
        }
    }

    Ok(resolved)
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

/// Read a file, optionally limited to a line range.
pub struct ReadTool {
    pub policy: Policy,
}

impl ReadTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports optional start/end line ranges."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "relative or absolute file path" },
                "start_line": { "type": "integer", "description": "1-based inclusive start line" },
                "end_line": { "type": "integer", "description": "1-based inclusive end line" }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'path' parameter".to_string()))?;
        let resolved = resolve_tool_path(&ctx, path, self.policy.workspace_only)?;

        let content = tokio::fs::read_to_string(&resolved).await.map_err(|e| {
            ToolError::Execution(format!("failed to read '{}': {}", resolved.display(), e))
        })?;

        if let Some(sink) = &ctx.viewed_files {
            if let Ok(mut guard) = sink.lock() {
                guard.insert(resolved.clone());
            }
        }

        let start_line = params["start_line"].as_u64().map(|v| v as usize);
        let end_line = params["end_line"].as_u64().map(|v| v as usize);

        let output = match (start_line, end_line) {
            (None, None) => content,
            _ => {
                let start = start_line.unwrap_or(1).saturating_sub(1);
                let end = end_line.unwrap_or(usize::MAX).saturating_sub(1);
                content
                    .lines()
                    .enumerate()
                    .filter_map(|(idx, line)| {
                        if idx >= start && idx <= end {
                            Some(line)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        Ok(ToolResult::ok(output))
    }
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

/// Write content to a file.
pub struct WriteTool {
    pub policy: Policy,
}

impl WriteTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write text content to a file, creating parent directories if needed."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "relative or absolute file path" },
                "content": { "type": "string", "description": "text content to write" }
            },
            "required": ["path", "content"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'path' parameter".to_string()))?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'content' parameter".to_string()))?;

        let resolved = resolve_tool_path(&ctx, path, self.policy.workspace_only)?;
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::Execution(format!(
                    "failed to create parent directories for '{}': {}",
                    resolved.display(),
                    e
                ))
            })?;
        }

        tokio::fs::write(&resolved, content).await.map_err(|e| {
            ToolError::Execution(format!("failed to write '{}': {}", resolved.display(), e))
        })?;

        Ok(ToolResult::ok(format!("wrote {}", resolved.display())))
    }
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

/// Replace an exact text occurrence in a file.
pub struct EditTool {
    pub policy: Policy,
}

impl EditTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact occurrence of old_string with new_string in a file."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn validate_input(&self, input: &serde_json::Value) -> Result<(), ToolError> {
        let old_string = input["old_string"].as_str().unwrap_or("");
        let new_string = input["new_string"].as_str().unwrap_or("");
        if old_string == new_string {
            return Err(ToolError::InvalidParams(
                "old_string and new_string are identical; nothing to replace".to_string(),
            ));
        }
        Ok(())
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'path' parameter".to_string()))?;
        let old_string = params["old_string"].as_str().ok_or_else(|| {
            ToolError::InvalidParams("missing 'old_string' parameter".to_string())
        })?;
        let new_string = params["new_string"].as_str().ok_or_else(|| {
            ToolError::InvalidParams("missing 'new_string' parameter".to_string())
        })?;

        let resolved = resolve_tool_path(&ctx, path, self.policy.workspace_only)?;
        let content = tokio::fs::read_to_string(&resolved).await.map_err(|e| {
            ToolError::Execution(format!("failed to read '{}': {}", resolved.display(), e))
        })?;

        if !content.contains(old_string) {
            return Err(ToolError::Execution(
                "old_string was not found in the file".to_string(),
            ));
        }

        let new_content = content.replacen(old_string, new_string, 1);
        tokio::fs::write(&resolved, new_content)
            .await
            .map_err(|e| {
                ToolError::Execution(format!("failed to write '{}': {}", resolved.display(), e))
            })?;

        Ok(ToolResult::ok(format!("edited {}", resolved.display())))
    }
}

// ---------------------------------------------------------------------------
// apply_patch
// ---------------------------------------------------------------------------

/// Apply a unified diff to a file.
pub struct ApplyPatchTool {
    pub policy: Policy,
}

impl ApplyPatchTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a unified diff to a file."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "target file path" },
                "diff": { "type": "string", "description": "unified diff content" }
            },
            "required": ["path", "diff"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'path' parameter".to_string()))?;
        let diff = params["diff"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'diff' parameter".to_string()))?;

        let resolved = resolve_tool_path(&ctx, path, self.policy.workspace_only)?;
        let content = tokio::fs::read_to_string(&resolved).await.map_err(|e| {
            ToolError::Execution(format!("failed to read '{}': {}", resolved.display(), e))
        })?;

        let new_content = apply_unified_diff(&content, diff).map_err(ToolError::Execution)?;
        tokio::fs::write(&resolved, new_content)
            .await
            .map_err(|e| {
                ToolError::Execution(format!("failed to write '{}': {}", resolved.display(), e))
            })?;

        Ok(ToolResult::ok(format!("patched {}", resolved.display())))
    }
}

fn apply_unified_diff(content: &str, diff: &str) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::with_capacity(lines.len());
    let mut line_idx = 0usize;

    enum State {
        Between,
        InHunk,
    }
    let mut state = State::Between;

    for raw_line in diff.lines() {
        // Header lines are ignored; the path comes from the tool parameter.
        if raw_line.starts_with("---") || raw_line.starts_with("+++") {
            continue;
        }

        if raw_line.starts_with("@@") && raw_line.ends_with("@@") {
            state = State::InHunk;
            // Parse the new-file start position so we know where to splice.
            let inner = raw_line
                .trim_start_matches('@')
                .trim_end_matches('@')
                .trim();
            // Format: "-start,count +start,count".
            let parts: Vec<&str> = inner.split_whitespace().collect();
            let plus = parts.get(1).ok_or("malformed hunk header")?;
            let start: usize = plus
                .split(',')
                .next()
                .ok_or("malformed hunk start")?
                .parse()
                .map_err(|_| "invalid hunk start")?;
            line_idx = start.saturating_sub(1);
            continue;
        }

        match state {
            State::Between => continue,
            State::InHunk => {
                if raw_line.is_empty() {
                    // Empty context lines are represented as " " in a diff, but a truly
                    // empty line in the hunk is treated as context.
                    if line_idx < lines.len() {
                        result.push(lines[line_idx].to_string());
                        line_idx += 1;
                    }
                } else {
                    let (marker, rest) = raw_line.split_at(1);
                    match marker {
                        " " => {
                            let expected = rest;
                            if line_idx >= lines.len() {
                                return Err(format!(
                                    "context line exceeds file length: {}",
                                    expected
                                ));
                            }
                            if lines[line_idx] != expected {
                                return Err(format!(
                                    "context mismatch at line {}: expected {:?}, got {:?}",
                                    line_idx + 1,
                                    expected,
                                    lines[line_idx]
                                ));
                            }
                            result.push(lines[line_idx].to_string());
                            line_idx += 1;
                        }
                        "-" => {
                            let expected = rest;
                            if line_idx >= lines.len() {
                                return Err(format!(
                                    "removal line exceeds file length: {}",
                                    expected
                                ));
                            }
                            if lines[line_idx] != expected {
                                return Err(format!(
                                    "removal mismatch at line {}: expected {:?}, got {:?}",
                                    line_idx + 1,
                                    expected,
                                    lines[line_idx]
                                ));
                            }
                            line_idx += 1;
                        }
                        "+" => {
                            result.push(rest.to_string());
                        }
                        "\\" => {
                            // "\ No newline at end of file" marker, ignored.
                        }
                        _ => {
                            return Err(format!("unknown diff marker: {}", marker));
                        }
                    }
                }
            }
        }
    }

    // Append any remaining lines after the last hunk.
    while line_idx < lines.len() {
        result.push(lines[line_idx].to_string());
        line_idx += 1;
    }

    Ok(result.join("\n"))
}

// ---------------------------------------------------------------------------
// exec
// ---------------------------------------------------------------------------

/// Execute a shell command and capture its output.
pub struct ExecTool {
    pub policy: Policy,
    backend: Arc<dyn SandboxBackend>,
}

impl ExecTool {
    pub fn new(policy: Policy) -> Self {
        Self::with_backend(policy, Arc::new(LocalSandboxBackend::new()))
    }

    pub fn with_backend(policy: Policy, backend: Arc<dyn SandboxBackend>) -> Self {
        Self { policy, backend }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout, stderr and exit code."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "shell command to execute" },
                "timeout": { "type": "integer", "description": "timeout in seconds (default 60)" }
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'command' parameter".to_string()))?;
        let timeout_secs = params["timeout"].as_u64().unwrap_or(60);

        let result = self
            .backend
            .exec(command, &ctx.workspace, timeout_secs)
            .await
            .map_err(|e| ToolError::Execution(format!("sandbox exec failed: {}", e)))?;

        let content = json!({
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
        })
        .to_string();

        Ok(ToolResult {
            content,
            is_error: result.exit_code != 0,
        })
    }
}

impl From<ExecResult> for ToolResult {
    fn from(result: ExecResult) -> Self {
        let content = json!({
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
        })
        .to_string();
        ToolResult {
            content,
            is_error: result.exit_code != 0,
        }
    }
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

/// Fetch a single web page and return its main text content.
pub struct WebFetchTool {
    pub policy: Policy,
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a single URL and return the stripped text content."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch" }
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let url = params["url"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'url' parameter".to_string()))?;

        let body = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("request failed: {}", e)))?
            .text()
            .await
            .map_err(|e| ToolError::Execution(format!("failed to read response: {}", e)))?;

        let text = strip_html(&body);
        Ok(ToolResult::ok(text))
    }
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

/// Search the web using DuckDuckGo.
pub struct WebSearchTool {
    pub policy: Policy,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using DuckDuckGo and return a list of result snippets."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "search query" },
                "count": { "type": "integer", "description": "maximum number of results (default 5)" }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let query = params["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'query' parameter".to_string()))?;
        let count = params["count"].as_u64().unwrap_or(5).min(10) as usize;

        let url =
            reqwest::Url::parse_with_params("https://lite.duckduckgo.com/lite/", [("q", query)])
                .map_err(|e| ToolError::Execution(format!("invalid search url: {}", e)))?;

        let body = self
            .client
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (compatible; Legion/0.1)")
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("search request failed: {}", e)))?
            .text()
            .await
            .map_err(|e| ToolError::Execution(format!("failed to read search response: {}", e)))?;

        let results = parse_duckduckgo_lite(&body, count);
        Ok(ToolResult::ok(results))
    }
}

// ---------------------------------------------------------------------------
// memory_search
// ---------------------------------------------------------------------------

/// Search the agent's persistent memory using semantic + keyword hybrid retrieval.
pub struct MemorySearchTool {
    policy: Policy,
}

impl MemorySearchTool {
    pub fn new() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Off,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

impl Default for MemorySearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search the agent's persistent memory using semantic and keyword retrieval."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "search query" },
                "top_k": { "type": "integer", "description": "maximum number of results (default 5)" }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let query = params["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'query' parameter".to_string()))?;
        let top_k = params["top_k"].as_u64().unwrap_or(5).max(1) as usize;

        let backend = ctx
            .memory
            .ok_or_else(|| ToolError::Execution("memory backend is not available".to_string()))?;

        let notes = backend
            .search(query, top_k)
            .await
            .map_err(|e| ToolError::Execution(format!("memory search failed: {e}")))?;

        if notes.is_empty() {
            return Ok(ToolResult::ok("No relevant memories found."));
        }

        let mut out = String::new();
        for note in notes {
            out.push_str(&format!(
                "## {} (score: {:.3})\n{}\n\n",
                note.id, note.score, note.content
            ));
        }
        Ok(ToolResult::ok(out.trim().to_string()))
    }
}

// ---------------------------------------------------------------------------
// memory_get
// ---------------------------------------------------------------------------

/// Read a memory file (or a line range from it).
pub struct MemoryGetTool {
    policy: Policy,
}

impl MemoryGetTool {
    pub fn new() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Off,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

impl Default for MemoryGetTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Read a memory file or a line range from it."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "memory file path" },
                "start_line": { "type": "integer", "description": "1-based inclusive start line" },
                "end_line": { "type": "integer", "description": "1-based inclusive end line" }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'path' parameter".to_string()))?;
        let start_line = params["start_line"].as_u64().map(|v| v as usize);
        let end_line = params["end_line"].as_u64().map(|v| v as usize);

        let backend = ctx
            .memory
            .ok_or_else(|| ToolError::Execution("memory backend is not available".to_string()))?;

        let range = match (start_line, end_line) {
            (Some(start), Some(end)) => Some((start - 1)..end),
            (Some(start), None) => Some((start - 1)..usize::MAX),
            (None, Some(end)) => Some(0..end),
            (None, None) => None,
        };

        let content = backend
            .get(path, range)
            .await
            .map_err(|e| ToolError::Execution(format!("memory get failed: {e}")))?;

        Ok(ToolResult::ok(content))
    }
}

// ---------------------------------------------------------------------------
// memory_index
// ---------------------------------------------------------------------------

/// Add or update a persistent memory entry in the agent's memory collection.
pub struct MemoryIndexTool {
    policy: Policy,
}

impl MemoryIndexTool {
    pub fn new() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Off,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

impl Default for MemoryIndexTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MemoryIndexTool {
    fn name(&self) -> &str {
        "memory_index"
    }

    fn description(&self) -> &str {
        "Add or update a persistent memory entry in the agent's memory collection."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "unique memory identifier" },
                "content": { "type": "string", "description": "memory content" },
                "kind": { "type": "string", "description": "entry type, e.g. fact, preference, decision" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "optional tags"
                },
                "source": { "type": "string", "description": "source file path, e.g. MEMORY.md" }
            },
            "required": ["id", "content"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let id = params["id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'id' parameter".to_string()))?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'content' parameter".to_string()))?;

        let backend = ctx
            .memory
            .ok_or_else(|| ToolError::Execution("memory backend is not available".to_string()))?;

        let meta = legion_runtime::memory::MemoryMeta {
            source: params["source"].as_str().map(|s| s.to_string()),
            kind: params["kind"].as_str().map(|s| s.to_string()),
            tags: params["tags"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            ..Default::default()
        };

        backend
            .index(id, content, meta)
            .await
            .map_err(|e| ToolError::Execution(format!("memory index failed: {e}")))?;

        Ok(ToolResult::ok(format!("indexed memory entry '{id}'")))
    }
}

fn parse_duckduckgo_lite(html: &str, count: usize) -> String {
    // DuckDuckGo lite wraps results in rows with class "result-link" or
    // "result-snippet".  We extract links and nearby snippets heuristically.
    let link_re =
        regex::Regex::new(r#"<a[^>]+class="result-link"[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#)
            .unwrap();
    let snippet_re =
        regex::Regex::new(r#"<td[^>]+class="result-snippet"[^>]*>(.*?)</td>"#).unwrap();

    let links: Vec<(String, String)> = link_re
        .captures_iter(html)
        .map(|cap| {
            let href = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let title = strip_html(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
            (href, title)
        })
        .collect();

    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|cap| strip_html(cap.get(1).map(|m| m.as_str()).unwrap_or("")))
        .collect();

    let mut out = String::new();
    for (i, (href, title)) in links.iter().take(count).enumerate() {
        let snippet = snippets.get(i).map(|s| s.as_str()).unwrap_or("");
        out.push_str(&format!("{}. {}\n{}\n{}\n\n", i + 1, title, href, snippet));
    }

    if out.is_empty() {
        out.push_str("No results found.");
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// HTML stripping
// ---------------------------------------------------------------------------

/// Strip HTML tags and decode common entities into plain text.
fn strip_html(html: &str) -> String {
    let tag_re = regex::Regex::new(r"<[^>]+>").unwrap();
    let mut text = tag_re.replace_all(html, " ").to_string();

    let entities = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&nbsp;", " "),
    ];
    for (ent, ch) in entities {
        text = text.replace(ent, ch);
    }

    // Collapse whitespace.
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// spawn_subagent (multi-agent Phase A)
// ---------------------------------------------------------------------------

/// Tool that delegates a sub-task to a child agent and returns its final text
/// as the tool result. The child runs with an isolated context (Typed), a
/// narrowed tool subset, and its own iteration/timeout/depth limits. The
/// spawner is supplied via `ToolContext::spawner`; when absent (e.g. tests or
/// an unwired runtime) the tool reports unavailability.
pub struct SpawnSubagentTool {
    policy: Policy,
}

impl SpawnSubagentTool {
    pub fn new() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Off,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

impl Default for SpawnSubagentTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn description(&self) -> &str {
        "Delegate a sub-task to a child agent with an isolated context and a \
         narrowed tool subset. Returns the child agent's final text. Use for \
         parallelizable research or focused sub-problems; prefer doing the work \
         directly when the task is small or needs the current conversation context."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["typed", "fork"],
                    "description": "Child kind: 'typed' (default) starts an isolated-context agent addressed by agent_type; 'fork' inherits this run's conversation history, workspace, and router."
                },
                "agent_type": {
                    "type": "string",
                    "description": "Child agent type (an entry in agents.list, or 'main'). Required for kind='typed'; ignored for kind='fork' (the fork always uses the parent agent type)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The task instruction for the child agent."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for the child run."
                },
                "allowed_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tool subset for the child; must be within the parent's set. MCP tools (mcp__*) are not passed down. Omit to inherit the parent's effective set."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Optional system prompt injected into the child run."
                },
                "max_iterations": {
                    "type": "integer",
                    "description": "Optional iteration cap override (default from subagents.defaultMaxIterations)."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional wall-clock timeout override in milliseconds."
                }
            },
            "required": ["prompt"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let kind_str = input
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("typed");
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;

        let (kind, history) = match kind_str {
            "typed" => {
                let agent_type = input
                    .get("agent_type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidParams(
                            "missing 'agent_type' parameter (required for kind='typed')"
                                .to_string(),
                        )
                    })?;
                (SubagentKind::Typed(agent_type.to_string()), Vec::new())
            }
            "fork" => {
                let snapshot = ctx
                    .parent_history
                    .as_ref()
                    .map(|h| h.as_ref().clone())
                    .unwrap_or_default();
                (SubagentKind::Fork, snapshot)
            }
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "unknown sub-agent kind '{other}' (expected 'typed' or 'fork')"
                )));
            }
        };

        // Resolve and validate the child's tool subset (permission narrowing).
        let child_allowed = resolve_child_allowed(&input, ctx.allowed_tools.as_deref())?;

        let model = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let system_prompt = input
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let max_iterations = input
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let timeout = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .map(std::time::Duration::from_millis);

        let spawner = ctx.spawner.ok_or_else(|| {
            ToolError::Execution("sub-agent spawning is not available".to_string())
        })?;

        let req = SubagentRequest {
            kind,
            prompt: prompt.to_string(),
            model,
            allowed_tools: child_allowed,
            parent_agent_id: ctx.agent_id.clone(),
            parent_depth: ctx.depth,
            system_prompt,
            history,
            max_iterations,
            timeout,
        };

        let handle = spawner
            .spawn(req)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let result = handle
            .join()
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(format_result(result))
    }
}

/// Compute the child's allowed-tool subset and enforce permission narrowing:
/// MCP tools are never passed down, and the child's set must be within the
/// parent's effective set (when the parent is itself narrowed).
fn resolve_child_allowed(
    input: &serde_json::Value,
    parent_allowed: Option<&[String]>,
) -> Result<Option<Vec<String>>, ToolError> {
    let child: Option<Vec<String>> = match input.get("allowed_tools") {
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| {
                ToolError::InvalidParams("'allowed_tools' must be an array of strings".to_string())
            })?;
            let mut names = Vec::with_capacity(arr.len());
            for item in arr {
                let name = item.as_str().ok_or_else(|| {
                    ToolError::InvalidParams("'allowed_tools' entries must be strings".to_string())
                })?;
                names.push(name.to_string());
            }
            Some(names)
        }
        // Inherit the parent's effective set when unspecified (None means the
        // parent is the unrestricted root, so the child is also unrestricted).
        None => parent_allowed.map(|p| p.to_vec()),
    };

    if let Some(names) = &child {
        validate_tool_subset(names, parent_allowed)?;
    }

    Ok(child)
}

/// Enforce permission narrowing on a child's tool set: MCP tools are never
/// passed down, and every name must be within the parent's effective set
/// (when the parent is itself narrowed).
fn validate_tool_subset(
    names: &[String],
    parent_allowed: Option<&[String]>,
) -> Result<(), ToolError> {
    for name in names {
        if name.starts_with("mcp__") {
            return Err(ToolError::InvalidParams(format!(
                "MCP tool '{name}' cannot be passed to a sub-agent"
            )));
        }
    }
    if let Some(parent) = parent_allowed {
        for name in names {
            if !parent.iter().any(|p| p == name) {
                return Err(ToolError::InvalidParams(format!(
                    "tool '{name}' is not in the parent agent's allowed set"
                )));
            }
        }
    }
    Ok(())
}

fn format_result(result: legion_runtime::SubagentResult) -> ToolResult {
    let suffix = result
        .transcript_path
        .as_ref()
        .map(|p| format!("\n(transcript: {})", p.display()))
        .unwrap_or_default();
    match result.status {
        SubagentStatus::Completed => ToolResult::ok(format!("{}{}", result.text, suffix)),
        SubagentStatus::Failed(err) => {
            ToolResult::ok(format!("[subagent failed] {err}\n{}{suffix}", result.text))
        }
        SubagentStatus::TimedOut => {
            ToolResult::ok(format!("[subagent timed_out] {}{suffix}", result.text))
        }
        SubagentStatus::Aborted => {
            ToolResult::ok(format!("[subagent aborted] {}{suffix}", result.text))
        }
    }
}

// ---------------------------------------------------------------------------
// agent_to_agent_send (tools-p1p2 Phase B)
// ---------------------------------------------------------------------------

/// Tool that delivers a fire-and-forget message to another agent, triggering
/// an asynchronous turn on the target. The messenger is supplied via
/// `ToolContext::messenger`; when absent (e.g. tests or an unwired runtime)
/// the tool reports unavailability.
///
/// Authorization is not decided here: the messenger enforces the target
/// agent's `allowFrom` list. This tool only rejects the degenerate
/// send-to-self case.
pub struct AgentToAgentSendTool {
    policy: Policy,
}

impl AgentToAgentSendTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for AgentToAgentSendTool {
    fn name(&self) -> &str {
        "agent_to_agent_send"
    }

    fn description(&self) -> &str {
        "Send a message to another agent, triggering an asynchronous turn on \
         that agent. Returns immediately with a delivery confirmation; the \
         target agent's reply is not awaited. Delivery only succeeds when the \
         target agent's allowFrom list includes this agent."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Target agent id (an entry in agents.list)."
                },
                "message": {
                    "type": "string",
                    "description": "The message to deliver to the target agent."
                }
            },
            "required": ["to", "message"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let to = input
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'to' parameter".to_string()))?;
        let message = input
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'message' parameter".to_string()))?;

        if to == ctx.agent_id {
            return Err(ToolError::Execution("cannot send to self".to_string()));
        }

        let messenger = ctx
            .messenger
            .ok_or_else(|| ToolError::Execution("agent messenger not wired".to_string()))?;

        let confirmation = messenger
            .send(&ctx.agent_id, to, message)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolResult::ok(confirmation))
    }
}

// ---------------------------------------------------------------------------
// run_coordinator (multi-agent Phase C)
// ---------------------------------------------------------------------------

/// Tool that executes a declared multi-phase coordinator plan: tasks within a
/// phase run concurrently as Typed sub-agents; phases run sequentially,
/// gated on their `depends_on` phases. Task prompts may use `{{results}}` to
/// receive the accumulated results of all previously completed phases.
pub struct RunCoordinatorTool {
    policy: Policy,
}

impl RunCoordinatorTool {
    pub fn new() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Off,
                permission_mode: None,
                allow_from: vec![],
                workspace_only: false,
            },
        }
    }
}

impl Default for RunCoordinatorTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for RunCoordinatorTool {
    fn name(&self) -> &str {
        "run_coordinator"
    }

    fn description(&self) -> &str {
        "Execute a multi-phase coordinator plan: each phase declares one or more \
         sub-agent tasks (run concurrently), and phases run in declaration order \
         after their `dependsOn` phases complete. Use `{{results}}` in a task \
         prompt to inject the accumulated results of previous phases (e.g. a \
         synthesis phase after parallel research). Prefer spawn_subagent for a \
         single delegation."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "phases": {
                    "type": "array",
                    "description": "Phases in declaration (topological) order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Unique phase name." },
                            "dependsOn": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Names of phases that must complete first (must be declared earlier)."
                            },
                            "tasks": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "agentType": { "type": "string", "description": "Child agent type (agents.list entry, or 'main')." },
                                        "prompt": { "type": "string", "description": "Task instruction; '{{results}}' injects prior phase results." },
                                        "model": { "type": "string" },
                                        "systemPrompt": { "type": "string" },
                                        "allowedTools": { "type": "array", "items": { "type": "string" }, "description": "Per-task tool subset (must be within the parent's set; mcp__* rejected)." },
                                        "maxIterations": { "type": "integer" },
                                        "timeoutMs": { "type": "integer" }
                                    },
                                    "required": ["agentType", "prompt"]
                                }
                            }
                        },
                        "required": ["name", "tasks"]
                    }
                }
            },
            "required": ["phases"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let plan: CoordinatorPlan = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidParams(format!("invalid coordinator plan: {e}")))?;

        // Permission narrowing applies to every task's tool subset.
        for phase in &plan.phases {
            for task in &phase.tasks {
                if let Some(names) = &task.allowed_tools {
                    validate_tool_subset(names, ctx.allowed_tools.as_deref())?;
                }
            }
        }

        let spawner = ctx.spawner.ok_or_else(|| {
            ToolError::Execution("sub-agent spawning is not available".to_string())
        })?;

        let report = run_coordinator_plan(&plan, &spawner, &ctx.agent_id, ctx.depth)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let task_count: usize = report.phases.iter().map(|p| p.results.len()).sum();
        Ok(ToolResult::ok(format!(
            "[coordinator] {} phase(s), {} task(s)\n{}",
            report.phases.len(),
            task_count,
            report.render()
        )))
    }
}

// ---------------------------------------------------------------------------
// swarm tools (multi-agent Phase D)
// ---------------------------------------------------------------------------

/// Tool that spawns a named, persistent teammate driven by a mailbox. The
/// teammate runs its first turn in the background; later turns are triggered
/// by `swarm_send` deliveries. The swarm manager is supplied via
/// `ToolContext::swarm`; when absent (e.g. tests or an unwired runtime) the
/// tool reports unavailability.
pub struct SwarmSpawnTool {
    policy: Policy,
}

impl SwarmSpawnTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for SwarmSpawnTool {
    fn name(&self) -> &str {
        "swarm_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a named teammate that works on a task in the background and \
         stays alive for follow-up messages via swarm_send. Use for \
         long-running parallel work you want to keep steering; prefer \
         spawn_subagent for one-shot delegation."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Unique teammate name (^[A-Za-z0-9._-]{1,32}$)."
                },
                "prompt": {
                    "type": "string",
                    "description": "Initial task instruction for the teammate."
                },
                "agentType": {
                    "type": "string",
                    "description": "Teammate agent type (an entry in agents.list, or 'main'). Defaults to this agent."
                },
                "allowedTools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tool subset for the teammate; must be within the parent's set. MCP tools (mcp__*) are not passed down. Omit to inherit the parent's effective set."
                }
            },
            "required": ["name", "prompt"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'name' parameter".to_string()))?;
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;
        let agent_type = input
            .get("agentType")
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.agent_id);

        // `resolve_child_allowed` reads the snake_case key used by
        // spawn_subagent; re-map the camelCase swarm key into the same shape
        // so the shared validation (subset check, mcp__ rejection) applies
        // unchanged.
        let mut resolved_input = input.clone();
        if let Some(tools) = resolved_input.get("allowedTools").cloned() {
            resolved_input["allowed_tools"] = tools;
        }
        let child_allowed = resolve_child_allowed(&resolved_input, ctx.allowed_tools.as_deref())?;

        let swarm = ctx
            .swarm
            .ok_or_else(|| ToolError::Execution("swarm is not available".to_string()))?;

        swarm
            .spawn_teammate(
                name,
                agent_type,
                prompt,
                &ctx.agent_id,
                ctx.depth,
                child_allowed,
            )
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        tracing::info!(teammate = name, agent_type, parent = %ctx.agent_id, "swarm teammate spawned");
        Ok(ToolResult::ok(format!(
            "teammate '{name}' spawned (agent type: {agent_type}); use swarm_send to steer it"
        )))
    }
}

/// Tool that queues a message in a teammate's mailbox, waking it when idle.
pub struct SwarmSendTool {
    policy: Policy,
}

impl SwarmSendTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for SwarmSendTool {
    fn name(&self) -> &str {
        "swarm_send"
    }

    fn description(&self) -> &str {
        "Send a message to a teammate's mailbox. An idle teammate wakes up \
         and runs a new turn with the queued messages; a running teammate \
         picks them up after its current turn. Returns a delivery \
         confirmation immediately."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Target teammate name (as given to swarm_spawn)."
                },
                "message": {
                    "type": "string",
                    "description": "The message to queue for the teammate."
                }
            },
            "required": ["to", "message"]
        })
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let to = input
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'to' parameter".to_string()))?;
        let message = input
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParams("missing 'message' parameter".to_string()))?;

        let swarm = ctx
            .swarm
            .ok_or_else(|| ToolError::Execution("swarm is not available".to_string()))?;

        swarm
            .send(&ctx.agent_id, to, message)
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        tracing::info!(from = %ctx.agent_id, to, "swarm message queued");
        Ok(ToolResult::ok(format!(
            "message queued for teammate '{to}'"
        )))
    }
}

/// Tool that reports the status of every teammate in the swarm (read-only).
pub struct SwarmStatusTool {
    policy: Policy,
}

impl SwarmStatusTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for SwarmStatusTool {
    fn name(&self) -> &str {
        "swarm_status"
    }

    fn description(&self) -> &str {
        "Show the status of all teammates: running/idle state, completed \
         turns, mailbox depth, and the latest result of each."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let swarm = ctx
            .swarm
            .ok_or_else(|| ToolError::Execution("swarm is not available".to_string()))?;

        let infos = swarm.status();
        if infos.is_empty() {
            return Ok(ToolResult::ok("no teammates"));
        }

        let mut out = String::new();
        for (idx, info) in infos.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            let status = match info.status {
                legion_runtime::TeammateStatus::Running => "running",
                legion_runtime::TeammateStatus::Idle => "idle",
            };
            out.push_str(&format!(
                "{} [{}] turns={} mailbox={}",
                info.name, status, info.turns, info.mailbox_depth
            ));
            let last = info
                .last_result
                .as_deref()
                .map(|r| truncate_chars(r, 300))
                .unwrap_or_else(|| "(none)".to_string());
            out.push_str(&format!("\n  last: {last}"));
        }
        Ok(ToolResult::ok(out))
    }
}

/// Truncate to at most `max` chars (char-safe) for status rendering.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_runtime::{MemoryBackend, MemoryNote, ToolContext};
    use std::collections::HashMap;
    use std::ops::Range;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::{MockServer, ResponseTemplate};

    fn ctx(dir: &TempDir, sender: Option<&str>) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            sender: sender.map(|s| s.to_string()),
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

    fn open_policy() -> Policy {
        Policy {
            approval: crate::policy::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    fn ws_only_policy() -> Policy {
        Policy {
            approval: crate::policy::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: true,
        }
    }

    #[tokio::test]
    async fn read_whole_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("foo.txt");
        tokio::fs::write(&file, "line one\nline two\n")
            .await
            .unwrap();

        let tool = ReadTool::new(open_policy());
        let res = tool
            .execute(json!({"path": "foo.txt"}), ctx(&dir, None))
            .await
            .unwrap();
        assert_eq!(res.content, "line one\nline two\n");
    }

    #[tokio::test]
    async fn read_with_line_range() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("foo.txt");
        tokio::fs::write(&file, "a\nb\nc\nd\n").await.unwrap();

        let tool = ReadTool::new(open_policy());
        let res = tool
            .execute(
                json!({"path": "foo.txt", "start_line": 2, "end_line": 3}),
                ctx(&dir, None),
            )
            .await
            .unwrap();
        assert_eq!(res.content, "b\nc");
    }

    #[tokio::test]
    async fn write_creates_file() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(open_policy());
        let res = tool
            .execute(
                json!({"path": "nested/bar.txt", "content": "hello"}),
                ctx(&dir, None),
            )
            .await
            .unwrap();
        assert!(res.content.contains("wrote"));
        let content = tokio::fs::read_to_string(dir.path().join("nested/bar.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn edit_replaces_text() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("x.txt");
        tokio::fs::write(&file, "alpha beta gamma").await.unwrap();

        let tool = EditTool::new(open_policy());
        tool.execute(
            json!({"path": "x.txt", "old_string": "beta", "new_string": "BETA"}),
            ctx(&dir, None),
        )
        .await
        .unwrap();

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(content, "alpha BETA gamma");
    }

    #[tokio::test]
    async fn apply_patch_changes_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("p.txt");
        tokio::fs::write(&file, "one\ntwo\nthree\n").await.unwrap();

        let diff = "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let tool = ApplyPatchTool::new(open_policy());
        tool.execute(json!({"path": "p.txt", "diff": diff}), ctx(&dir, None))
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(content, "one\nTWO\nthree");
    }

    #[test]
    fn apply_unified_diff_rejects_context_mismatch() {
        let content = "one\ntwo\nthree\n";
        let diff = "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 +1,3 @@\n one\n WRONG\n three\n";
        let err = apply_unified_diff(content, diff).unwrap_err();
        assert!(
            err.contains("context mismatch"),
            "expected context mismatch, got: {err}"
        );
    }

    #[test]
    fn apply_unified_diff_rejects_removal_mismatch() {
        let content = "one\ntwo\nthree\n";
        let diff = "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 +1,3 @@\n one\n-WRONG\n three\n";
        let err = apply_unified_diff(content, diff).unwrap_err();
        assert!(
            err.contains("removal mismatch"),
            "expected removal mismatch, got: {err}"
        );
    }

    #[test]
    fn apply_unified_diff_rejects_unknown_marker() {
        let content = "one\ntwo\nthree\n";
        let diff = "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 +1,3 @@\n one\n@bogus\n";
        let err = apply_unified_diff(content, diff).unwrap_err();
        assert!(
            err.contains("unknown diff marker"),
            "expected unknown marker error, got: {err}"
        );
    }

    #[test]
    fn apply_unified_diff_rejects_malformed_hunk_header() {
        let content = "one\ntwo\nthree\n";
        let diff = "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 @@\n one\n";
        let err = apply_unified_diff(content, diff).unwrap_err();
        assert!(
            err.contains("malformed hunk header"),
            "expected malformed hunk header, got: {err}"
        );
    }

    #[tokio::test]
    async fn workspace_only_rejects_escape() {
        let dir = TempDir::new().unwrap();
        let tool = ReadTool::new(ws_only_policy());
        let res = tool
            .execute(json!({"path": "../etc/passwd"}), ctx(&dir, None))
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn exec_returns_output() {
        let dir = TempDir::new().unwrap();
        let tool = ExecTool::new(open_policy());
        let res = tool
            .execute(
                json!({"command": "echo hello && echo err >&2 && exit 42"}),
                ctx(&dir, None),
            )
            .await
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&res.content).unwrap();
        assert!(parsed["stdout"].as_str().unwrap().contains("hello"));
        assert!(parsed["stderr"].as_str().unwrap().contains("err"));
        assert_eq!(parsed["exit_code"].as_i64(), Some(42));
        assert!(res.is_error);
    }

    #[test]
    fn strip_html_works() {
        let html = "<p>Hello &amp; <b>world</b></p>";
        assert_eq!(strip_html(html), "Hello & world");
    }

    #[tokio::test]
    async fn web_fetch_with_wiremock() {
        let server = MockServer::start().await;
        let body = "<html><body><p>Hello world</p></body></html>";
        server
            .register(
                wiremock::Mock::given(method("GET"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(body)),
            )
            .await;

        let dir = TempDir::new().unwrap();
        let tool = WebFetchTool::new(open_policy());
        let res = tool
            .execute(json!({"url": server.uri()}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(res.content.contains("Hello world"));
    }

    #[test]
    fn read_tool_is_read_only_and_concurrency_safe() {
        let tool = ReadTool::new(open_policy());
        let input = json!({"path": "x.txt"});
        assert!(tool.is_read_only(&input));
        assert!(tool.is_concurrency_safe(&input));
    }

    #[test]
    fn write_tool_is_not_read_only_and_not_concurrency_safe() {
        let tool = WriteTool::new(open_policy());
        let input = json!({"path": "x.txt", "content": "hi"});
        assert!(!tool.is_read_only(&input));
        assert!(!tool.is_concurrency_safe(&input));
    }

    #[test]
    fn edit_tool_rejects_identical_strings() {
        let tool = EditTool::new(open_policy());
        let err = tool
            .validate_input(&json!({
                "path": "x.txt",
                "old_string": "same",
                "new_string": "same"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("identical"));
    }

    #[test]
    fn exec_tool_is_not_concurrency_safe() {
        let tool = ExecTool::new(open_policy());
        let input = json!({"command": "echo hi"});
        assert!(!tool.is_concurrency_safe(&input));
        assert!(!tool.is_read_only(&input));
    }

    #[test]
    fn web_tools_are_read_only_and_concurrency_safe() {
        let fetch = WebFetchTool::new(open_policy());
        let search = WebSearchTool::new(open_policy());
        assert!(fetch.is_read_only(&json!({"url": "http://x"})));
        assert!(fetch.is_concurrency_safe(&json!({"url": "http://x"})));
        assert!(search.is_read_only(&json!({"query": "x"})));
        assert!(search.is_concurrency_safe(&json!({"query": "x"})));
    }

    #[derive(Default)]
    struct FakeMemoryBackend {
        files: Mutex<HashMap<String, String>>,
        notes: Mutex<Vec<MemoryNote>>,
    }

    impl FakeMemoryBackend {
        fn with_note(self, note: MemoryNote) -> Self {
            self.notes.lock().unwrap().push(note);
            self
        }

        fn with_file(self, path: impl Into<String>, content: impl Into<String>) -> Self {
            self.files
                .lock()
                .unwrap()
                .insert(path.into(), content.into());
            self
        }

        fn note_ids(&self) -> Vec<String> {
            self.notes
                .lock()
                .unwrap()
                .iter()
                .map(|n| n.id.clone())
                .collect()
        }
    }

    #[async_trait]
    impl MemoryBackend for FakeMemoryBackend {
        async fn search(
            &self,
            query: &str,
            top_k: usize,
        ) -> Result<Vec<MemoryNote>, legion_runtime::MemoryError> {
            let notes = self.notes.lock().unwrap().clone();
            Ok(notes
                .into_iter()
                .filter(|n| n.content.contains(query))
                .take(top_k)
                .collect())
        }

        async fn get(
            &self,
            path: &str,
            range: Option<Range<usize>>,
        ) -> Result<String, legion_runtime::MemoryError> {
            let files = self.files.lock().unwrap();
            let content = files.get(path).cloned().ok_or_else(|| {
                legion_runtime::MemoryError::GetFailed(format!("not found: {path}"))
            })?;
            match range {
                Some(r) => {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = r.start.min(lines.len());
                    let end = r.end.min(lines.len());
                    Ok(lines[start..end].join("\n"))
                }
                None => Ok(content),
            }
        }

        async fn index(
            &self,
            id: &str,
            content: &str,
            _meta: legion_runtime::memory::MemoryMeta,
        ) -> Result<(), legion_runtime::MemoryError> {
            let mut notes = self.notes.lock().unwrap();
            if let Some(existing) = notes.iter_mut().find(|n| n.id == id) {
                existing.content = content.to_string();
            } else {
                notes.push(MemoryNote {
                    id: id.to_string(),
                    content: content.to_string(),
                    score: 1.0,
                    kind: None,
                });
            }
            Ok(())
        }
    }

    fn ctx_with_memory(dir: &TempDir, memory: Arc<dyn MemoryBackend>) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            sender: None,
            memory: Some(memory),
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
    async fn memory_search_returns_matching_notes() {
        let dir = TempDir::new().unwrap();
        let memory = Arc::new(
            FakeMemoryBackend::default()
                .with_note(MemoryNote {
                    id: "note-1".into(),
                    content: "User prefers dark mode.".into(),
                    score: 0.9,
                    kind: None,
                })
                .with_note(MemoryNote {
                    id: "note-2".into(),
                    content: "Project uses Rust.".into(),
                    score: 0.8,
                    kind: None,
                }),
        );

        let tool = MemorySearchTool::new();
        let res = tool
            .execute(json!({"query": "dark"}), ctx_with_memory(&dir, memory))
            .await
            .unwrap();

        assert!(res.content.contains("note-1"));
        assert!(res.content.contains("dark mode"));
        assert!(!res.content.contains("note-2"));
    }

    #[tokio::test]
    async fn memory_search_requires_backend() {
        let dir = TempDir::new().unwrap();
        let tool = MemorySearchTool::new();
        let res = tool.execute(json!({"query": "x"}), ctx(&dir, None)).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn memory_get_reads_file_and_range() {
        let dir = TempDir::new().unwrap();
        let memory = Arc::new(
            FakeMemoryBackend::default().with_file("MEMORY.md", "line one\nline two\nline three\n"),
        );

        let tool = MemoryGetTool::new();
        let res = tool
            .execute(
                json!({"path": "MEMORY.md", "start_line": 2, "end_line": 2}),
                ctx_with_memory(&dir, memory),
            )
            .await
            .unwrap();
        assert_eq!(res.content, "line two");
    }

    #[tokio::test]
    async fn memory_get_requires_backend() {
        let dir = TempDir::new().unwrap();
        let tool = MemoryGetTool::new();
        let res = tool
            .execute(json!({"path": "MEMORY.md"}), ctx(&dir, None))
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn memory_index_adds_searchable_note() {
        let dir = TempDir::new().unwrap();
        let memory = Arc::new(FakeMemoryBackend::default());
        let tool = MemoryIndexTool::new();

        tool.execute(
            json!({
                "id": "pref-1",
                "content": "User prefers dark mode.",
                "kind": "preference",
                "tags": ["ui"]
            }),
            ctx_with_memory(&dir, memory.clone()),
        )
        .await
        .unwrap();

        assert!(memory.note_ids().contains(&"pref-1".to_string()));

        let search = MemorySearchTool::new();
        let res = search
            .execute(json!({"query": "dark"}), ctx_with_memory(&dir, memory))
            .await
            .unwrap();
        assert!(res.content.contains("dark mode"));
    }

    #[tokio::test]
    async fn memory_index_requires_backend() {
        let dir = TempDir::new().unwrap();
        let tool = MemoryIndexTool::new();
        let res = tool
            .execute(json!({"id": "x", "content": "y"}), ctx(&dir, None))
            .await;
        assert!(res.is_err());
    }

    #[test]
    fn resolve_child_allowed_inherits_parent_when_unspecified() {
        let parent = vec!["read".to_string(), "write".to_string()];
        let got = resolve_child_allowed(&json!({}), Some(&parent)).unwrap();
        assert_eq!(got, Some(parent));
    }

    #[test]
    fn resolve_child_allowed_unrestricted_root_yields_none() {
        let got = resolve_child_allowed(&json!({}), None).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn resolve_child_allowed_rejects_mcp_tools() {
        let err = resolve_child_allowed(&json!({"allowed_tools": ["mcp__fs__read"]}), None)
            .expect_err("mcp tools must not pass down");
        assert!(err.to_string().contains("MCP tool"));
    }

    #[test]
    fn resolve_child_allowed_rejects_tools_outside_parent_set() {
        let parent = vec!["read".to_string()];
        let err = resolve_child_allowed(&json!({"allowed_tools": ["write"]}), Some(&parent))
            .expect_err("child must be a subset of the parent");
        assert!(err.to_string().contains("not in the parent"));
    }

    #[test]
    fn resolve_child_allowed_accepts_subset_of_parent() {
        let parent = vec!["read".to_string(), "write".to_string()];
        let got =
            resolve_child_allowed(&json!({"allowed_tools": ["read"]}), Some(&parent)).unwrap();
        assert_eq!(got, Some(vec!["read".to_string()]));
    }

    #[tokio::test]
    async fn spawn_subagent_typed_requires_agent_type() {
        let dir = TempDir::new().unwrap();
        let tool = SpawnSubagentTool::new();
        let res = tool.execute(json!({"prompt": "x"}), ctx(&dir, None)).await;
        let err = res.expect_err("typed spawn without agent_type must fail");
        assert!(err.to_string().contains("agent_type"), "got {err}");
    }

    #[tokio::test]
    async fn spawn_subagent_rejects_unknown_kind() {
        let dir = TempDir::new().unwrap();
        let tool = SpawnSubagentTool::new();
        let res = tool
            .execute(json!({"kind": "bogus", "prompt": "x"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("unknown kind must fail");
        assert!(
            err.to_string().contains("unknown sub-agent kind"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn spawn_subagent_fork_does_not_require_agent_type() {
        // Fork parses without agent_type and proceeds until the (absent)
        // spawner is consulted, proving agent_type is not required for forks.
        let dir = TempDir::new().unwrap();
        let tool = SpawnSubagentTool::new();
        let res = tool
            .execute(json!({"kind": "fork", "prompt": "x"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("no spawner wired in this test ctx");
        assert!(
            err.to_string().contains("spawning is not available"),
            "fork should reach the spawner lookup, got {err}"
        );
    }

    #[derive(Default)]
    struct FakeSpawner {
        requests: Mutex<Vec<legion_runtime::SubagentRequest>>,
    }

    #[async_trait]
    impl legion_runtime::SubagentSpawner for FakeSpawner {
        async fn spawn(
            &self,
            req: legion_runtime::SubagentRequest,
        ) -> Result<legion_runtime::SubagentHandle, legion_runtime::SubagentError> {
            self.requests.lock().unwrap().push(req);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(legion_runtime::SubagentResult {
                handle_id: "h".to_string(),
                text: "coordinated-output".to_string(),
                tool_call_count: 0,
                transcript_path: None,
                status: legion_runtime::SubagentStatus::Completed,
            });
            Ok(legion_runtime::SubagentHandle::from_receiver(
                "h".to_string(),
                rx,
            ))
        }
    }

    fn ctx_with_spawner(
        dir: &TempDir,
        spawner: Arc<dyn legion_runtime::SubagentSpawner>,
        allowed_tools: Option<Vec<String>>,
    ) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "main".to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools,
            spawner: Some(spawner),
            messenger: None,
            swarm: None,
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
        }
    }

    fn plan_input() -> serde_json::Value {
        json!({
            "phases": [
                {
                    "name": "research",
                    "tasks": [
                        { "agentType": "researcher", "prompt": "gather facts" },
                        { "agentType": "researcher", "prompt": "gather more" }
                    ]
                },
                {
                    "name": "synthesis",
                    "dependsOn": ["research"],
                    "tasks": [
                        { "agentType": "writer", "prompt": "summarize: {{results}}" }
                    ]
                }
            ]
        })
    }

    #[tokio::test]
    async fn run_coordinator_executes_plan_and_returns_report() {
        let dir = TempDir::new().unwrap();
        let spawner = Arc::new(FakeSpawner::default());
        let tool = RunCoordinatorTool::new();
        let res = tool
            .execute(plan_input(), ctx_with_spawner(&dir, spawner.clone(), None))
            .await
            .unwrap();

        assert!(
            res.content.contains("[coordinator] 2 phase(s), 3 task(s)"),
            "got {}",
            res.content
        );
        assert!(res.content.contains("coordinated-output"));

        let reqs = spawner.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert!(
            reqs[2].prompt.contains("coordinated-output"),
            "synthesis prompt must receive accumulated results, got {:?}",
            reqs[2].prompt
        );
        assert!(reqs.iter().all(|r| r.parent_agent_id == "main"));
    }

    #[tokio::test]
    async fn run_coordinator_rejects_task_tools_outside_parent_set() {
        let dir = TempDir::new().unwrap();
        let spawner = Arc::new(FakeSpawner::default());
        let mut plan = plan_input();
        plan["phases"][0]["tasks"][0]["allowedTools"] = json!(["write"]);
        let tool = RunCoordinatorTool::new();
        let res = tool
            .execute(
                plan,
                ctx_with_spawner(&dir, spawner, Some(vec!["read".to_string()])),
            )
            .await;
        let err = res.expect_err("task subset must be within the parent's set");
        assert!(err.to_string().contains("not in the parent"), "got {err}");
    }

    #[tokio::test]
    async fn run_coordinator_requires_spawner() {
        let dir = TempDir::new().unwrap();
        let tool = RunCoordinatorTool::new();
        let res = tool.execute(plan_input(), ctx(&dir, None)).await;
        let err = res.expect_err("no spawner wired in this test ctx");
        assert!(
            err.to_string().contains("spawning is not available"),
            "got {err}"
        );
    }

    // -----------------------------------------------------------------------
    // agent_to_agent_send (tools-p1p2 Phase B)
    // -----------------------------------------------------------------------

    /// Test messenger that records every delivery and can be configured to
    /// reject with a specific error.
    struct RecordingMessenger {
        calls: std::sync::Mutex<Vec<(String, String, String)>>,
        fail_with: Option<legion_runtime::MessengerError>,
    }

    impl RecordingMessenger {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(err: legion_runtime::MessengerError) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_with: Some(err),
            }
        }
    }

    #[async_trait]
    impl legion_runtime::AgentMessenger for RecordingMessenger {
        async fn send(
            &self,
            from_agent: &str,
            to_agent: &str,
            message: &str,
        ) -> Result<String, legion_runtime::MessengerError> {
            if let Some(err) = &self.fail_with {
                return Err(legion_runtime::MessengerError::Runtime(err.to_string()));
            }
            self.calls.lock().unwrap().push((
                from_agent.to_string(),
                to_agent.to_string(),
                message.to_string(),
            ));
            Ok(format!("delivered to {to_agent} (async)"))
        }
    }

    fn a2a_tool() -> AgentToAgentSendTool {
        AgentToAgentSendTool::new(Policy {
            approval: Approval::Prompt,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }

    fn ctx_with_messenger(
        dir: &TempDir,
        messenger: Arc<dyn legion_runtime::AgentMessenger>,
    ) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "main".to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: Some(messenger),
            swarm: None,
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
        }
    }

    #[tokio::test]
    async fn a2a_send_delivers_and_records_call() {
        let dir = TempDir::new().unwrap();
        let messenger = Arc::new(RecordingMessenger::new());
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "researcher", "message": "please review this"}),
                ctx_with_messenger(&dir, messenger.clone()),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        assert_eq!(res.content, "delivered to researcher (async)");
        let calls = messenger.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(
                "main".to_string(),
                "researcher".to_string(),
                "please review this".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a2a_send_requires_messenger() {
        let dir = TempDir::new().unwrap();
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "researcher", "message": "hi"}),
                ctx(&dir, None),
            )
            .await;
        let err = res.expect_err("no messenger wired in the default test ctx");
        assert!(
            err.to_string().contains("agent messenger not wired"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn a2a_send_rejects_self_send() {
        let dir = TempDir::new().unwrap();
        let messenger = Arc::new(RecordingMessenger::new());
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "main", "message": "note to self"}),
                ctx_with_messenger(&dir, messenger.clone()),
            )
            .await;
        let err = res.expect_err("send-to-self must be rejected");
        assert!(err.to_string().contains("cannot send to self"), "got {err}");
        assert!(messenger.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a2a_send_propagates_not_allowed() {
        let dir = TempDir::new().unwrap();
        let messenger = Arc::new(RecordingMessenger::failing(
            legion_runtime::MessengerError::NotAllowed {
                from: "main".to_string(),
                to: "researcher".to_string(),
            },
        ));
        let tool = a2a_tool();
        let res = tool
            .execute(
                json!({"to": "researcher", "message": "hi"}),
                ctx_with_messenger(&dir, messenger),
            )
            .await;
        let err = res.expect_err("NotAllowed must surface as a tool error");
        assert!(err.to_string().contains("not allowed"), "got {err}");
    }

    // -----------------------------------------------------------------------
    // swarm tools (multi-agent Phase D)
    // -----------------------------------------------------------------------

    fn prompt_policy() -> Policy {
        Policy {
            approval: Approval::Prompt,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    fn ctx_with_swarm(dir: &TempDir, swarm: Arc<legion_runtime::SwarmManager>) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "main".to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: None,
            swarm: Some(swarm),
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
        }
    }

    #[tokio::test]
    async fn swarm_spawn_requires_swarm() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSpawnTool::new(prompt_policy());
        let res = tool
            .execute(
                json!({"name": "worker", "prompt": "do things"}),
                ctx(&dir, None),
            )
            .await;
        let err = res.expect_err("no swarm wired in the default test ctx");
        assert!(
            err.to_string().contains("swarm is not available"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_send_requires_swarm() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSendTool::new(prompt_policy());
        let res = tool
            .execute(json!({"to": "worker", "message": "hi"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("no swarm wired in the default test ctx");
        assert!(
            err.to_string().contains("swarm is not available"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_status_requires_swarm() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmStatusTool::new(open_policy());
        let res = tool.execute(json!({}), ctx(&dir, None)).await;
        let err = res.expect_err("no swarm wired in the default test ctx");
        assert!(
            err.to_string().contains("swarm is not available"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_spawn_requires_name_and_prompt() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSpawnTool::new(prompt_policy());

        let res = tool
            .execute(json!({"prompt": "do things"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("missing name must fail");
        assert!(err.to_string().contains("'name'"), "got {err}");

        let res = tool
            .execute(json!({"name": "worker"}), ctx(&dir, None))
            .await;
        let err = res.expect_err("missing prompt must fail");
        assert!(err.to_string().contains("'prompt'"), "got {err}");
    }

    #[tokio::test]
    async fn swarm_spawn_rejects_mcp_tools() {
        let dir = TempDir::new().unwrap();
        let tool = SwarmSpawnTool::new(prompt_policy());
        let res = tool
            .execute(
                json!({
                    "name": "worker",
                    "prompt": "do things",
                    "allowedTools": ["mcp__fs__read"]
                }),
                ctx(&dir, None),
            )
            .await;
        let err = res.expect_err("mcp tools must be rejected");
        assert!(
            err.to_string().contains("cannot be passed to a sub-agent"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn swarm_spawn_and_status_roundtrip() {
        let dir = TempDir::new().unwrap();
        let swarm = Arc::new(legion_runtime::SwarmManager::new(Arc::new(
            FakeSpawner::default(),
        )));
        let spawn = SwarmSpawnTool::new(prompt_policy());
        let res = spawn
            .execute(
                json!({"name": "worker", "prompt": "research this"}),
                ctx_with_swarm(&dir, swarm.clone()),
            )
            .await
            .unwrap();
        assert!(
            res.content.contains("teammate 'worker' spawned"),
            "got {}",
            res.content
        );

        // Wait for the first turn to complete, then check the status output.
        for _ in 0..200 {
            if swarm
                .status()
                .iter()
                .any(|t| t.name == "worker" && t.turns == 1)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let status = SwarmStatusTool::new(open_policy());
        let res = status
            .execute(json!({}), ctx_with_swarm(&dir, swarm))
            .await
            .unwrap();
        assert!(
            res.content.contains("worker [idle] turns=1 mailbox=0"),
            "got {}",
            res.content
        );
        assert!(
            res.content.contains("last: coordinated-output"),
            "got {}",
            res.content
        );
    }
}
