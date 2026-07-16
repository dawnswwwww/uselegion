//! Todo list tool: lets the agent update a visible session checklist.

use async_trait::async_trait;
use legion_runtime::{TodoItem, TodoList, Tool, ToolContext, ToolError, ToolResult};
use serde_json::json;

use crate::policy::Policy;

/// Update the session todo list.
pub struct TodoWriteTool {
    policy: Policy,
}

impl TodoWriteTool {
    /// Create a new todo-write tool with the given policy.
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Update the session todo list. Call this whenever you start, complete, \
         or rephase a multi-step task so the user can see your progress. \
         Pass the full updated list each time."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete updated todo list. Replaces any previous list.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable identifier for this todo item."
                            },
                            "content": {
                                "type": "string",
                                "description": "Short description shown in the UI."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current status of the item."
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Present-continuous phrase used while in_progress, e.g. 'Running tests'."
                            }
                        },
                        "required": ["id", "content", "status", "activeForm"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        // Updating the todo list does not mutate the workspace.
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
        let store = ctx
            .todo_store
            .ok_or_else(|| ToolError::Execution("todo store is not available".to_string()))?;

        let items = match params.get("todos") {
            Some(v) => serde_json::from_value::<Vec<TodoItem>>(v.clone())
                .map_err(|e| ToolError::InvalidParams(format!("invalid todos: {e}")))?,
            None => {
                return Err(ToolError::InvalidParams(
                    "missing 'todos' parameter".to_string(),
                ));
            }
        };

        let list = TodoList { items };
        store
            .save(&list)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolResult::ok(format!(
            "Todo list updated: {} items, {} completed",
            list.items.len(),
            list.completed_count()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::{JsonTodoStore, TodoStatus, TodoStore};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn policy() -> Policy {
        Policy {
            approval: crate::policy::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    fn ctx_with_store(store: Arc<dyn TodoStore>) -> ToolContext {
        ToolContext {
            workspace: TempDir::new().unwrap().path().to_path_buf(),
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
            todo_store: Some(store),
        }
    }

    #[tokio::test]
    async fn writes_todo_list() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(
            JsonTodoStore::open(dir.path().join("todos.json"))
                .await
                .unwrap(),
        );
        let tool = TodoWriteTool::new(policy());

        let result = tool
            .execute(
                json!({
                    "todos": [
                        {"id": "1", "content": "read spec", "status": "in_progress", "activeForm": "Reading spec"},
                        {"id": "2", "content": "write code", "status": "pending", "activeForm": "Writing code"}
                    ]
                }),
                ctx_with_store(store.clone()),
            )
            .await
            .unwrap();

        assert!(result.content.contains("2 items"));
        assert!(result.content.contains("0 completed"));

        let list = store.load().await.unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].status, TodoStatus::InProgress);
        assert_eq!(list.items[1].status, TodoStatus::Pending);
    }

    #[tokio::test]
    async fn rejects_missing_todos() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(
            JsonTodoStore::open(dir.path().join("todos.json"))
                .await
                .unwrap(),
        );
        let tool = TodoWriteTool::new(policy());

        let err = tool
            .execute(json!({}), ctx_with_store(store))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'todos'"));
    }

    #[tokio::test]
    async fn rejects_invalid_status() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(
            JsonTodoStore::open(dir.path().join("todos.json"))
                .await
                .unwrap(),
        );
        let tool = TodoWriteTool::new(policy());

        let err = tool
            .execute(
                json!({
                    "todos": [
                        {"id": "1", "content": "x", "status": "done", "activeForm": "X"}
                    ]
                }),
                ctx_with_store(store),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid todos"));
    }
}
