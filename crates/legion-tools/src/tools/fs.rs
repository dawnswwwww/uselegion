use std::path::{Path, PathBuf};

use async_trait::async_trait;
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde_json::json;

use crate::policy::Policy;

/// Helper to stamp `kind()` and `namespace()` on a built-in Legion tool.
macro_rules! legion_tool_taxonomy {
    ($kind:expr) => {
        fn kind(&self) -> ToolKind {
            $kind
        }
        fn namespace(&self) -> ToolNamespace {
            ToolNamespace::Legion
        }
    };
}

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

    legion_tool_taxonomy!(ToolKind::Read);

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

    legion_tool_taxonomy!(ToolKind::Write);

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

    legion_tool_taxonomy!(ToolKind::Edit);

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

    legion_tool_taxonomy!(ToolKind::Edit);

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
            background_tasks: None,
            plan_mode_tracker: None,
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
}
