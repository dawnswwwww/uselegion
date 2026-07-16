use std::path::Path;

use async_trait::async_trait;
use glob::Pattern;
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use regex::Regex;
use serde_json::json;

use crate::policy::Policy;
use crate::tools::resolve_tool_path;

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
// grep
// ---------------------------------------------------------------------------

const MAX_MATCH_LINES: usize = 1000;
const MAX_MATCH_BYTES: usize = 40 * 1024;

/// Search files for a literal or regex pattern.
pub struct GrepTool {
    pub policy: Policy,
}

impl GrepTool {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search for a pattern in files. Supports regex and literal matching, \
         optional directory recursion, and glob filtering."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "regex or literal search pattern"
                },
                "path": {
                    "type": "string",
                    "description": "file or directory to search (defaults to workspace root)"
                },
                "glob": {
                    "type": "string",
                    "description": "file name glob filter, e.g. '*.rs'"
                },
                "regex": {
                    "type": "boolean",
                    "description": "treat pattern as a regex (default true); false for literal"
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    legion_tool_taxonomy!(ToolKind::Search);

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'pattern' parameter".to_string()))?;
        let path = params["path"].as_str().unwrap_or("");
        let glob_filter = params["glob"].as_str();
        let use_regex = params["regex"].as_bool().unwrap_or(true);

        let resolved = resolve_tool_path(&ctx, path, self.policy.workspace_only)?;

        let matcher = if use_regex {
            Matcher::Regex(
                Regex::new(pattern)
                    .map_err(|e| ToolError::InvalidParams(format!("invalid regex: {e}")))?,
            )
        } else {
            Matcher::Literal(pattern.to_string())
        };

        let glob_pattern = glob_filter
            .map(|g| {
                Pattern::new(g).map_err(|e| ToolError::InvalidParams(format!("invalid glob: {e}")))
            })
            .transpose()?;

        let mut matches = Vec::new();
        let mut truncated = false;

        if resolved.is_file() {
            search_file(
                &ctx.workspace,
                &resolved,
                &matcher,
                &mut matches,
                &mut truncated,
            )
            .await?;
        } else if resolved.is_dir() {
            walk_dir(
                &ctx.workspace,
                &resolved,
                &matcher,
                glob_pattern.as_ref(),
                &mut matches,
                &mut truncated,
            )
            .await?;
        } else {
            return Err(ToolError::Execution(format!(
                "path '{}' does not exist or is not a searchable file or directory",
                resolved.display()
            )));
        }

        let mut output = matches.join("\n");
        if truncated {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!(
                "... output truncated (limit: {MAX_MATCH_LINES} lines or {}KB)",
                MAX_MATCH_BYTES / 1024
            ));
        } else if matches.is_empty() {
            output = "No matches found.".to_string();
        }

        Ok(ToolResult::ok(output))
    }
}

enum Matcher {
    Regex(Regex),
    Literal(String),
}

impl Matcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Matcher::Regex(re) => re.is_match(line),
            Matcher::Literal(s) => line.contains(s),
        }
    }
}

fn relative_path<'a>(workspace: &'a Path, file: &'a Path) -> &'a Path {
    file.strip_prefix(workspace).unwrap_or(file)
}

async fn search_file(
    workspace: &Path,
    file: &Path,
    matcher: &Matcher,
    matches: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<(), ToolError> {
    if *truncated {
        return Ok(());
    }

    let content = match tokio::fs::read_to_string(file).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %file.display(), error = %e, "grep: skipped unreadable file");
            return Ok(());
        }
    };

    let rel = relative_path(workspace, file);
    for (idx, line) in content.lines().enumerate() {
        if matcher.matches(line) {
            if would_exceed_limits(matches, line.len()) {
                *truncated = true;
                break;
            }
            matches.push(format!("{}:{}:{}", rel.display(), idx + 1, line));
        }
    }

    Ok(())
}

fn would_exceed_limits(matches: &[String], next_line_len: usize) -> bool {
    if matches.len() >= MAX_MATCH_LINES {
        return true;
    }
    let bytes: usize = matches.iter().map(|m| m.len()).sum();
    // Account for newline separators that will be added when joining.
    let separators = matches.len().saturating_sub(1);
    bytes + separators + next_line_len > MAX_MATCH_BYTES
}

async fn walk_dir(
    workspace: &Path,
    dir: &Path,
    matcher: &Matcher,
    glob: Option<&Pattern>,
    matches: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<(), ToolError> {
    if *truncated {
        return Ok(());
    }

    let mut entries = tokio::fs::read_dir(dir).await.map_err(|e| {
        ToolError::Execution(format!(
            "failed to read directory '{}': {}",
            dir.display(),
            e
        ))
    })?;

    while let Some(entry) = entries.next_entry().await.map_err(|e| {
        ToolError::Execution(format!(
            "failed to read entry in directory '{}': {}",
            dir.display(),
            e
        ))
    })? {
        if *truncated {
            return Ok(());
        }

        let name = entry.file_name();
        let name_lossy = name.to_string_lossy();
        if name_lossy.starts_with('.') {
            continue;
        }

        let path = entry.path();
        let file_type = entry.file_type().await.map_err(|e| {
            ToolError::Execution(format!(
                "failed to get file type for '{}': {}",
                path.display(),
                e
            ))
        })?;

        if file_type.is_dir() {
            Box::pin(walk_dir(
                workspace, &path, matcher, glob, matches, truncated,
            ))
            .await?;
        } else if file_type.is_file() {
            if let Some(pattern) = glob {
                if !pattern.matches(&name_lossy) {
                    continue;
                }
            }
            search_file(workspace, &path, matcher, matches, truncated).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Approval;
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
            plan_mode_tracker: None,
        }
    }

    fn off_policy() -> Policy {
        Policy {
            approval: Approval::Off,
            allow_from: vec![],
            workspace_only: false,
            permission_mode: None,
        }
    }

    fn ws_only_policy() -> Policy {
        Policy {
            approval: Approval::Off,
            allow_from: vec![],
            workspace_only: true,
            permission_mode: None,
        }
    }

    #[tokio::test]
    async fn single_file_match() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("foo.txt"), "apple\nbanana\napricot\n")
            .await
            .unwrap();

        let tool = GrepTool::new(off_policy());
        let res = tool
            .execute(
                json!({"pattern": "a", "path": "foo.txt", "regex": false}),
                ctx(&dir),
            )
            .await
            .unwrap();

        assert!(res.content.contains("foo.txt:1:apple"));
        assert!(res.content.contains("foo.txt:2:banana"));
        assert!(res.content.contains("foo.txt:3:apricot"));
    }

    #[tokio::test]
    async fn regex_vs_literal() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("data.txt"), "foo 123 bar\nfoo 456 bar\n")
            .await
            .unwrap();

        let tool = GrepTool::new(off_policy());

        let regex_res = tool
            .execute(
                json!({"pattern": r"foo \d+ bar", "path": "data.txt"}),
                ctx(&dir),
            )
            .await
            .unwrap();
        assert!(regex_res.content.contains("data.txt:1:foo 123 bar"));
        assert!(regex_res.content.contains("data.txt:2:foo 456 bar"));

        let literal_res = tool
            .execute(
                json!({"pattern": r"foo \d+ bar", "path": "data.txt", "regex": false}),
                ctx(&dir),
            )
            .await
            .unwrap();
        assert_eq!(literal_res.content, "No matches found.");
    }

    #[tokio::test]
    async fn glob_filtering() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "fn beta() {}\n")
            .await
            .unwrap();

        let tool = GrepTool::new(off_policy());
        let res = tool
            .execute(json!({"pattern": "fn", "glob": "*.rs"}), ctx(&dir))
            .await
            .unwrap();

        assert!(res.content.contains("a.rs:1:fn alpha() {}"));
        assert!(!res.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn directory_recursion() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir(dir.path().join("nested"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("root.txt"), "root match\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("nested/deep.txt"), "deep match\n")
            .await
            .unwrap();
        // Hidden files/directories are skipped.
        tokio::fs::create_dir(dir.path().join(".hidden"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(".hidden/secret.txt"), "secret match\n")
            .await
            .unwrap();

        let tool = GrepTool::new(off_policy());
        let res = tool
            .execute(json!({"pattern": "match", "regex": false}), ctx(&dir))
            .await
            .unwrap();

        assert!(res.content.contains("root.txt:1:root match"));
        assert!(res.content.contains("nested/deep.txt:1:deep match"));
        assert!(!res.content.contains("secret"));
    }

    #[tokio::test]
    async fn empty_results_message() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("x.txt"), "hello\n")
            .await
            .unwrap();

        let tool = GrepTool::new(off_policy());
        let res = tool
            .execute(
                json!({"pattern": "notfound", "path": "x.txt", "regex": false}),
                ctx(&dir),
            )
            .await
            .unwrap();

        assert_eq!(res.content, "No matches found.");
    }

    #[tokio::test]
    async fn workspace_only_rejects_parent_escape() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("safe.txt"), "secret\n")
            .await
            .unwrap();

        let tool = GrepTool::new(ws_only_policy());
        let res = tool
            .execute(
                json!({"pattern": "secret", "path": "../safe.txt"}),
                ctx(&dir),
            )
            .await;

        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("'..'"));
    }
}
