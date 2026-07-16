//! Session-level todo list for tracking multi-step agent work.
//!
//! The todo list is model-driven: the agent updates it via the `todo_write`
//! tool. It is persisted per `(agent_id, session_id)` so it survives TUI
//! reconnects and can be shared between gateway clients watching the same
//! session.

use futures::SinkExt;
use futures::channel::mpsc::Sender;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::types::RunEvent;

/// Lifecycle status of a single todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started yet.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// A single task in the session todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    /// Stable identifier used when rewriting the list.
    pub id: String,
    /// Short description shown in the TUI.
    pub content: String,
    /// Current status.
    pub status: TodoStatus,
    /// Present-continuous phrase used while the item is in progress,
    /// e.g. "Running tests".
    pub active_form: String,
}

/// The full todo list for a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

impl TodoList {
    /// Returns true when there are no items at all.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns true when at least one item is not completed.
    pub fn has_incomplete(&self) -> bool {
        self.items.iter().any(|t| t.status != TodoStatus::Completed)
    }

    /// Count of completed items.
    pub fn completed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count()
    }
}

/// Errors that can occur when interacting with the todo store.
#[derive(Debug, Error)]
pub enum TodoStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Storage backend for a session todo list.
#[async_trait::async_trait]
pub trait TodoStore: Send + Sync {
    /// Load the current todo list, returning an empty list if none exists.
    async fn load(&self) -> Result<TodoList, TodoStoreError>;

    /// Persist the todo list, replacing any previous contents.
    async fn save(&self, list: &TodoList) -> Result<(), TodoStoreError>;
}

/// A thread-safe boxed todo store.
pub type SharedTodoStore = Arc<dyn TodoStore>;

/// File-backed todo store using an atomic JSON write.
pub struct JsonTodoStore {
    path: PathBuf,
    /// In-memory cache serialized by a mutex so concurrent writes within the
    /// same process are ordered. Cross-process safety relies on the atomic
    /// temp-then-rename write.
    cache: Mutex<Option<TodoList>>,
    /// Optional event sink. When present, every successful `save` emits a
    /// `RunEvent::TodoUpdate` so the TUI/gateway can refresh immediately.
    event_tx: Option<Mutex<Sender<RunEvent>>>,
}

impl JsonTodoStore {
    /// Open or create a todo store at the given path.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, TodoStoreError> {
        Self::open_with_event_tx(path, None).await
    }

    /// Open or create a todo store, optionally wiring it to a runtime event
    /// channel so that writes are immediately broadcast to observers.
    pub async fn open_with_event_tx(
        path: impl AsRef<Path>,
        event_tx: Option<Sender<RunEvent>>,
    ) -> Result<Self, TodoStoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let initial = Self::load_from_disk(&path).await?;
        Ok(Self {
            path,
            cache: Mutex::new(Some(initial)),
            event_tx: event_tx.map(Mutex::new),
        })
    }

    /// Build a store path from agent and session identifiers.
    pub fn path_for(base: &Path, agent_id: &str, session_id: &str) -> PathBuf {
        base.join("agents")
            .join(sanitize_path_component(agent_id))
            .join("todos")
            .join(format!("{}.json", sanitize_path_component(session_id)))
    }

    async fn load_from_disk(path: &Path) -> Result<TodoList, TodoStoreError> {
        if !path.exists() {
            return Ok(TodoList::default());
        }
        let content = tokio::fs::read_to_string(path).await?;
        if content.trim().is_empty() {
            return Ok(TodoList::default());
        }
        serde_json::from_str(&content).map_err(TodoStoreError::Json)
    }

    async fn write_atomically(path: &Path, list: &TodoList) -> Result<(), TodoStoreError> {
        let tmp = tmp_path_for(path);
        let written = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let payload = serde_json::to_vec_pretty(list)?;
            file.write_all(&payload).await?;
            file.flush().await?;
            Ok::<(), TodoStoreError>(())
        }
        .await;

        if let Err(err) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(err);
        }

        if let Err(err) = tokio::fs::rename(&tmp, path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(err.into());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl TodoStore for JsonTodoStore {
    async fn load(&self) -> Result<TodoList, TodoStoreError> {
        let cache = self.cache.lock().await;
        Ok(cache.as_ref().cloned().unwrap_or_default())
    }

    async fn save(&self, list: &TodoList) -> Result<(), TodoStoreError> {
        Self::write_atomically(&self.path, list).await?;
        let mut cache = self.cache.lock().await;
        *cache = Some(list.clone());
        drop(cache);

        if let Some(tx) = &self.event_tx {
            let mut tx = tx.lock().await;
            let _ = tx.send(RunEvent::TodoUpdate { list: list.clone() }).await;
        }
        Ok(())
    }
}

/// Sanitize a string for safe use as a file path component.
fn sanitize_path_component(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Unique temp path next to `path` for atomic write-then-rename persistence.
fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!("{file_name}.tmp-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_store_returns_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonTodoStore::open(dir.path().join("todos.json"))
            .await
            .unwrap();
        let list = store.load().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("todos.json");
        let store = JsonTodoStore::open(&path).await.unwrap();

        let list = TodoList {
            items: vec![TodoItem {
                id: "1".to_string(),
                content: "read spec".to_string(),
                status: TodoStatus::InProgress,
                active_form: "Reading spec".to_string(),
            }],
        };
        store.save(&list).await.unwrap();

        let reloaded = JsonTodoStore::open(&path).await.unwrap();
        assert_eq!(reloaded.load().await.unwrap(), list);
    }

    #[tokio::test]
    async fn atomic_write_leaves_no_temp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("todos.json");
        let store = JsonTodoStore::open(&path).await.unwrap();
        store.save(&TodoList::default()).await.unwrap();

        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut residue = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".tmp-") {
                residue.push(name);
            }
        }
        assert!(residue.is_empty(), "temp files left behind: {residue:?}");
    }
}
