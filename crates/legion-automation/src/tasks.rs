//! Minimal task ledger for detached work.
//!
//! Tasks record the lifecycle of background work such as cron executions,
//! subagent runs, ACP delegations, and CLI-triggered jobs. The MVP store is a
//! simple append-only JSONL file; production implementations can swap in SQLite.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// The kind of background work represented by a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Cron,
    Subagent,
    Acp,
    Cli,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::Cron => "cron",
            TaskKind::Subagent => "subagent",
            TaskKind::Acp => "acp",
            TaskKind::Cli => "cli",
        }
    }
}

impl AsRef<str> for TaskKind {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// The status of a task in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Timeout,
}

/// A single background task record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional message/prompt used when running this task through an agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// IDs of tasks that must complete before this one can run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

impl Task {
    /// Create a new pending task.
    pub fn new(id: impl Into<String>, kind: TaskKind, agent_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            status: TaskStatus::Pending,
            agent_id: agent_id.into(),
            session_id: None,
            run_id: None,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
            error: None,
            message: None,
            depends_on: Vec::new(),
        }
    }

    /// Set the message/prompt for this task.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Add a dependency task id.
    pub fn depends_on(mut self, id: impl Into<String>) -> Self {
        self.depends_on.push(id.into());
        self
    }

    /// Mark the task as running.
    pub fn mark_running(&mut self) {
        self.status = TaskStatus::Running;
        self.started_at = Some(Utc::now());
    }

    /// Mark the task as completed.
    pub fn mark_completed(&mut self) {
        self.status = TaskStatus::Completed;
        self.ended_at = Some(Utc::now());
    }

    /// Mark the task as failed with an optional error message.
    pub fn mark_failed(&mut self, error: impl Into<String>) {
        self.status = TaskStatus::Failed;
        self.ended_at = Some(Utc::now());
        self.error = Some(error.into());
    }
}

/// Errors that can occur when interacting with the task store.
#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("task '{0}' not found")]
    NotFound(String),
}

/// Storage backend for task records.
#[async_trait::async_trait]
pub trait TaskStore: Send + Sync {
    /// Persist a new task record.
    async fn create(&self, task: Task) -> Result<(), TaskStoreError>;

    /// Update an existing task record in place.
    async fn update(&self, task: Task) -> Result<(), TaskStoreError>;

    /// List all tasks, newest first.
    async fn list(&self) -> Result<Vec<Task>, TaskStoreError>;

    /// Fetch a single task by id.
    async fn get(&self, id: &str) -> Result<Option<Task>, TaskStoreError>;

    /// List tasks that are currently pending, oldest first.
    async fn list_pending(&self) -> Result<Vec<Task>, TaskStoreError>;
}

/// File-backed task store using an append-only JSONL log.
pub struct JsonlTaskStore {
    path: PathBuf,
    tasks: Mutex<Vec<Task>>,
}

impl JsonlTaskStore {
    /// Open or create a JSONL task store at the given path.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, TaskStoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tasks = Self::load(&path).await?;
        Ok(Self {
            path,
            tasks: Mutex::new(tasks),
        })
    }

    async fn load(path: &Path) -> Result<Vec<Task>, TaskStoreError> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = tokio::fs::File::open(path).await?;
        let reader = tokio::io::BufReader::new(file);
        let mut lines = reader.lines();
        let mut tasks = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Task>(&line) {
                Ok(task) => tasks.push(task),
                Err(err) => {
                    tracing::warn!(line = %line, error = %err, "skipping malformed task record")
                }
            }
        }
        Ok(tasks)
    }

    async fn save(&self, tasks: &[Task]) -> Result<(), TaskStoreError> {
        // Crash-safe write: serialize into a uniquely-named temp file in the
        // same directory, then rename over the target so a crash mid-write
        // never leaves a truncated store behind.
        let tmp = tmp_path_for(&self.path);
        let written = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            for task in tasks {
                let line = serde_json::to_string(task)?;
                file.write_all(line.as_bytes()).await?;
                file.write_all(b"\n").await?;
            }
            file.flush().await?;
            Ok::<(), TaskStoreError>(())
        }
        .await;
        if let Err(err) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(err);
        }
        if let Err(err) = tokio::fs::rename(&tmp, &self.path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(err.into());
        }
        Ok(())
    }
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

#[async_trait::async_trait]
impl TaskStore for JsonlTaskStore {
    async fn create(&self, task: Task) -> Result<(), TaskStoreError> {
        let mut tasks = self.tasks.lock().await;
        tasks.push(task);
        self.save(&tasks).await
    }

    async fn update(&self, task: Task) -> Result<(), TaskStoreError> {
        let mut tasks = self.tasks.lock().await;
        let pos = tasks
            .iter()
            .position(|t| t.id == task.id)
            .ok_or_else(|| TaskStoreError::NotFound(task.id.clone()))?;
        tasks[pos] = task;
        self.save(&tasks).await
    }

    async fn list(&self) -> Result<Vec<Task>, TaskStoreError> {
        let tasks = self.tasks.lock().await;
        let mut out = tasks.clone();
        out.reverse();
        Ok(out)
    }

    async fn get(&self, id: &str) -> Result<Option<Task>, TaskStoreError> {
        let tasks = self.tasks.lock().await;
        Ok(tasks.iter().find(|t| t.id == id).cloned())
    }

    async fn list_pending(&self) -> Result<Vec<Task>, TaskStoreError> {
        let tasks = self.tasks.lock().await;
        let mut pending: Vec<Task> = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .cloned()
            .collect();
        pending.sort_by_key(|a| a.created_at);
        Ok(pending)
    }
}

/// A thread-safe boxed task store.
pub type SharedTaskStore = Arc<dyn TaskStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_create_and_list_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlTaskStore::open(dir.path().join("tasks.jsonl"))
            .await
            .unwrap();

        let task = Task::new("task-1", TaskKind::Cron, "main");
        store.create(task.clone()).await.unwrap();

        let tasks = store.list().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task-1");
        assert_eq!(tasks[0].kind, TaskKind::Cron);
        assert_eq!(tasks[0].status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn should_update_task_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlTaskStore::open(dir.path().join("tasks.jsonl"))
            .await
            .unwrap();

        let mut task = Task::new("task-2", TaskKind::Cron, "main");
        store.create(task.clone()).await.unwrap();

        task.mark_running();
        store.update(task.clone()).await.unwrap();

        let fetched = store.get("task-2").await.unwrap().expect("task exists");
        assert_eq!(fetched.status, TaskStatus::Running);
        assert!(fetched.started_at.is_some());
    }

    #[tokio::test]
    async fn should_persist_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.jsonl");

        {
            let store = JsonlTaskStore::open(&path).await.unwrap();
            store
                .create(Task::new("task-3", TaskKind::Cli, "work"))
                .await
                .unwrap();
        }

        {
            let store = JsonlTaskStore::open(&path).await.unwrap();
            let tasks = store.list().await.unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].id, "task-3");
        }
    }

    #[tokio::test]
    async fn save_writes_atomically_without_temp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.jsonl");
        let store = JsonlTaskStore::open(&path).await.unwrap();
        store
            .create(Task::new("task-atomic", TaskKind::Cli, "main"))
            .await
            .unwrap();

        // The target file holds the full record.
        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let parsed: Task = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed.id, "task-atomic");

        // No temp residue remains in the same directory.
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut residue = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            if entry.file_name().to_string_lossy().contains(".tmp-") {
                residue.push(entry.file_name());
            }
        }
        assert!(residue.is_empty(), "temp files left behind: {residue:?}");
    }
}
