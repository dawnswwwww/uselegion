use async_trait::async_trait;
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde_json::json;

use crate::policy::{Approval, Policy};

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

    legion_tool_taxonomy!(ToolKind::MemorySearch);

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

    legion_tool_taxonomy!(ToolKind::MemoryGet);

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

    legion_tool_taxonomy!(ToolKind::Skill);

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

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::{MemoryBackend, MemoryNote};
    use std::collections::HashMap;
    use std::ops::Range;
    use std::sync::{Arc, Mutex};
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
            background_tasks: None,
            plan_mode_tracker: None,
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
}
