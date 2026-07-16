use std::path::{Path, PathBuf};

use async_trait::async_trait;
use legion_runtime::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::json;

use crate::policy::Policy;
use crate::tools::resolve_tool_path;

/// Maximum recursion depth for `list_dir` when `recursive` is true.
const DEFAULT_MAX_DEPTH: usize = 3;

/// A single directory entry collected during listing.
struct DirEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

/// List the contents of a directory.
pub struct ListDirTool {
    pub policy: Policy,
}

impl ListDirTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. Supports optional recursive listing."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "relative or absolute directory path" },
                "recursive": { "type": "boolean", "description": "list recursively up to a bounded depth (default false)" }
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
        let recursive = params["recursive"].as_bool().unwrap_or(false);

        let resolved = resolve_tool_path(&ctx, path, self.policy.workspace_only)?;

        let metadata = tokio::fs::metadata(&resolved).await.map_err(|e| {
            ToolError::Execution(format!("failed to access '{}': {}", resolved.display(), e))
        })?;

        if !metadata.is_dir() {
            return Err(ToolError::Execution(format!(
                "'{}' is not a directory",
                resolved.display()
            )));
        }

        let entries = if recursive {
            list_recursive(&resolved, DEFAULT_MAX_DEPTH).await?
        } else {
            list_immediate(&resolved).await?
        };

        if entries.is_empty() {
            return Ok(ToolResult::ok("(empty directory)".to_string()));
        }

        Ok(ToolResult::ok(entries.join("\n")))
    }
}

/// Collect entries in `dir` and return them sorted by name.
async fn collect_entries(dir: &Path) -> Result<Vec<DirEntry>, ToolError> {
    let mut entries = Vec::new();
    let mut reader = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| ToolError::Execution(format!("failed to read directory: {e}")))?;

    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| ToolError::Execution(format!("failed to read directory entry: {e}")))?
    {
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .await
            .map_err(|e| {
                ToolError::Execution(format!(
                    "failed to determine file type for '{}': {}",
                    path.display(),
                    e
                ))
            })?
            .is_dir();
        let name = entry.file_name().to_string_lossy().to_string();
        entries.push(DirEntry { name, path, is_dir });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// List immediate children of `dir`, sorted alphabetically.
async fn list_immediate(dir: &Path) -> Result<Vec<String>, ToolError> {
    let entries = collect_entries(dir).await?;
    Ok(entries
        .into_iter()
        .map(|e| format!("{} {}", if e.is_dir { "[DIR]" } else { "[FILE]" }, e.name))
        .collect())
}

/// Recursively list children of `dir` up to `max_depth`, returning relative
/// paths prefixed with `[DIR]` or `[FILE]`.
async fn list_recursive(dir: &Path, max_depth: usize) -> Result<Vec<String>, ToolError> {
    let mut result = Vec::new();
    collect_recursive(dir, dir, 0, max_depth, &mut result).await?;
    Ok(result)
}

async fn collect_recursive(
    root: &Path,
    current: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
) -> Result<(), ToolError> {
    if depth > max_depth {
        return Ok(());
    }

    let entries = collect_entries(current).await?;

    for entry in entries {
        let relative = entry
            .path
            .strip_prefix(root)
            .unwrap_or(&entry.path)
            .display()
            .to_string();
        out.push(format!(
            "{} {}",
            if entry.is_dir { "[DIR]" } else { "[FILE]" },
            relative
        ));

        if entry.is_dir && depth < max_depth {
            Box::pin(collect_recursive(
                root,
                &entry.path,
                depth + 1,
                max_depth,
                out,
            ))
            .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Approval;
    use legion_runtime::ToolContext;
    use serde_json::json;
    use tempfile::TempDir;

    fn open_policy() -> Policy {
        Policy {
            approval: Approval::Off,
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
        }
    }

    #[tokio::test]
    async fn list_dir_shows_immediate_children() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "a")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub/b.txt"), "b")
            .await
            .unwrap();

        let tool = ListDirTool::new(open_policy());
        let res = tool.execute(json!({"path": "."}), ctx(&dir)).await.unwrap();

        assert!(res.content.contains("[FILE] a.txt"));
        assert!(res.content.contains("[DIR] sub"));
        assert!(!res.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn list_dir_recursive_lists_nested_entries() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "a")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
        tokio::fs::write(dir.path().join("sub/b.txt"), "b")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub/nested"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("sub/nested/c.txt"), "c")
            .await
            .unwrap();

        let tool = ListDirTool::new(open_policy());
        let res = tool
            .execute(json!({"path": ".", "recursive": true}), ctx(&dir))
            .await
            .unwrap();

        assert!(res.content.contains("[FILE] a.txt"));
        assert!(res.content.contains("[DIR] sub"));
        assert!(res.content.contains("[FILE] sub/b.txt"));
        assert!(res.content.contains("[DIR] sub/nested"));
        assert!(res.content.contains("[FILE] sub/nested/c.txt"));
    }

    #[tokio::test]
    async fn list_dir_missing_directory_returns_error() {
        let dir = TempDir::new().unwrap();
        let tool = ListDirTool::new(open_policy());
        let res = tool
            .execute(json!({"path": "does_not_exist"}), ctx(&dir))
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("failed to access"), "got {err}");
    }

    #[tokio::test]
    async fn list_dir_rejects_file_path() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "a")
            .await
            .unwrap();

        let tool = ListDirTool::new(open_policy());
        let res = tool.execute(json!({"path": "a.txt"}), ctx(&dir)).await;
        let err = res.expect_err("file path must be rejected");
        assert!(err.to_string().contains("is not a directory"));
    }
}
