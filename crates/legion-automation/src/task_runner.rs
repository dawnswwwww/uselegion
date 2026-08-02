//! Generic background task runner and task-flow engine.
//!
//! The runner polls pending tasks from a [`TaskStore`], respects simple
//! dependencies (`depends_on`), executes each task through the agent runtime,
//! and updates the task record with the outcome.

use crate::tasks::{SharedTaskStore, Task, TaskKind, TaskStatus, TaskStoreError};
use chrono::Utc;
use futures::StreamExt;
use legion_provider::model_ref::resolve_agent_model;
use legion_runtime::{Harness, RunRequest};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tracing::{info, warn};

/// Errors that can occur in the task runner.
#[derive(Debug, Error)]
pub enum TaskRunnerError {
    #[error("task store error: {0}")]
    TaskStore(#[from] TaskStoreError),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("task '{0}' not found")]
    NotFound(String),
    #[error("task '{0}' has unmet dependencies")]
    DependenciesNotMet(String),
}

/// Request to enqueue a new background task.
#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    pub agent_id: String,
    pub message: String,
    pub kind: TaskKind,
    pub depends_on: Vec<String>,
}

/// Generic background task runner.
pub struct TaskRunner {
    pub task_store: SharedTaskStore,
    pub runtime: Arc<dyn Harness>,
    pub config: legion_core::config::Config,
}

impl TaskRunner {
    pub fn new(
        task_store: SharedTaskStore,
        runtime: Arc<dyn Harness>,
        config: legion_core::config::Config,
    ) -> Self {
        Self {
            task_store,
            runtime,
            config,
        }
    }

    /// Enqueue a new pending task.
    pub async fn enqueue(&self, req: EnqueueRequest) -> Result<Task, TaskRunnerError> {
        let id = format!(
            "task-{}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0),
            rand_task_suffix()
        );
        let task = Task::new(&id, req.kind, &req.agent_id)
            .with_message(&req.message)
            .depends_on_multiple(req.depends_on);
        self.task_store.create(task.clone()).await?;
        Ok(task)
    }

    /// Run a single task by id, regardless of its current status.
    pub async fn run(&self, id: &str) -> Result<Task, TaskRunnerError> {
        let task = self
            .task_store
            .get(id)
            .await?
            .ok_or_else(|| TaskRunnerError::NotFound(id.to_string()))?;
        self.execute(task).await
    }

    /// Poll pending tasks and execute those whose dependencies are met.
    pub async fn process_pending(&self) -> Vec<Result<Task, TaskRunnerError>> {
        let pending = match self.task_store.list_pending().await {
            Ok(tasks) => tasks,
            Err(err) => return vec![Err(err.into())],
        };

        if pending.is_empty() {
            return Vec::new();
        }

        let all_task_ids: HashSet<String> = match self.task_store.list().await {
            Ok(tasks) => tasks.into_iter().map(|t| t.id).collect(),
            Err(err) => return vec![Err(err.into())],
        };
        let completed_ids: HashSet<String> = match self.task_store.list().await {
            Ok(tasks) => tasks
                .into_iter()
                .filter(|t| t.status == TaskStatus::Completed)
                .map(|t| t.id)
                .collect(),
            Err(err) => return vec![Err(err.into())],
        };

        let mut results = Vec::new();
        for task in pending {
            if !task.depends_on.is_empty() {
                // All dependencies must exist and be completed.
                let deps_exist = task.depends_on.iter().all(|d| all_task_ids.contains(d));
                let deps_completed = task.depends_on.iter().all(|d| completed_ids.contains(d));
                if !deps_exist {
                    let mut failed = task;
                    failed.mark_failed("dependency task does not exist");
                    if let Err(err) = self.task_store.update(failed.clone()).await {
                        results.push(Err(err.into()));
                    } else {
                        results.push(Err(TaskRunnerError::Runtime(format!(
                            "task '{}' has missing dependencies",
                            failed.id
                        ))));
                    }
                    continue;
                }
                if !deps_completed {
                    results.push(Err(TaskRunnerError::DependenciesNotMet(task.id.clone())));
                    continue;
                }
            }
            results.push(self.execute(task).await);
        }
        results
    }

    /// Background loop that polls for pending tasks every `interval`.
    pub async fn background_loop(self: Arc<Self>, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let results = self.process_pending().await;
            for result in results {
                match result {
                    Ok(task) => {
                        info!(task_id = %task.id, status = ?task.status, "background task executed")
                    }
                    Err(TaskRunnerError::DependenciesNotMet(id)) => {
                        info!(task_id = %id, "background task waiting on dependencies")
                    }
                    Err(err) => warn!(error = %err, "background task failed"),
                }
            }
        }
    }

    async fn execute(&self, mut task: Task) -> Result<Task, TaskRunnerError> {
        task.mark_running();
        self.task_store.update(task.clone()).await?;

        let model_ref = resolve_agent_model(&self.config, &task.agent_id);
        let session_id = task
            .session_id
            .clone()
            .unwrap_or_else(|| session_key_for_task(&task));
        let message = task.message.clone().unwrap_or_default();
        let request = RunRequest::new(&session_id, &task.agent_id, &message, model_ref)
            .with_system_prompt(format!(
                "You are executing a background task ({}). Complete the following instruction:",
                task.id
            ));

        let mut stream = match self.runtime.run(request) {
            Ok(stream) => stream,
            Err(err) => {
                task.mark_failed(err.to_string());
                self.task_store.update(task.clone()).await?;
                return Err(TaskRunnerError::Runtime(err.to_string()));
            }
        };

        let mut saw_error = None;
        while let Some(event) = stream.next().await {
            if let legion_runtime::RunEvent::Lifecycle {
                phase: legion_runtime::LifecyclePhase::Error,
                error,
            } = event
            {
                saw_error = error;
                break;
            }
        }

        if let Some(error) = saw_error {
            task.mark_failed(error);
        } else {
            task.mark_completed();
        }
        task.session_id = Some(session_id);
        task.run_id = Some(task.id.clone());
        self.task_store.update(task.clone()).await?;
        Ok(task)
    }
}

fn session_key_for_task(task: &Task) -> String {
    legion_plugin_sdk::session_key::direct_session_key(
        &task.agent_id,
        "task",
        task.kind.as_ref(),
        "default",
        &task.id,
    )
}

fn rand_task_suffix() -> u32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed) as u32
}

trait TaskDependsOn {
    fn depends_on_multiple(self, ids: Vec<String>) -> Self;
}

impl TaskDependsOn for Task {
    fn depends_on_multiple(mut self, ids: Vec<String>) -> Self {
        self.depends_on = ids;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{JsonlTaskStore, TaskKind, TaskStatus};
    use legion_runtime::{LifecyclePhase, RunEvent, RunStream, RuntimeError};

    struct FakeHarness {
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Harness for FakeHarness {
        fn id(&self) -> &str {
            "fake"
        }

        fn can_handle(&self, _model_ref: &str) -> bool {
            true
        }

        fn run(&self, _request: RunRequest) -> Result<RunStream, RuntimeError> {
            if self.fail {
                Ok(Box::pin(futures::stream::iter(vec![RunEvent::Lifecycle {
                    phase: LifecyclePhase::Error,
                    error: Some("boom".to_string()),
                }])))
            } else {
                Ok(Box::pin(futures::stream::iter(vec![
                    RunEvent::Lifecycle {
                        phase: LifecyclePhase::Start,
                        error: None,
                    },
                    RunEvent::AssistantDelta {
                        delta: "ok".to_string(),
                    },
                    RunEvent::Lifecycle {
                        phase: LifecyclePhase::End,
                        error: None,
                    },
                ])))
            }
        }
    }

    async fn test_runner(fail: bool) -> (TaskRunner, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let task_store: SharedTaskStore = Arc::new(
            JsonlTaskStore::open(dir.path().join("tasks.jsonl"))
                .await
                .unwrap(),
        );
        let config = legion_core::config::Config::from_json(
            r#"{ "gateway": { "auth": { "token": "x" } } }"#,
        )
        .unwrap();
        let runner = TaskRunner::new(task_store, Arc::new(FakeHarness { fail }), config);
        (runner, dir)
    }

    #[tokio::test]
    async fn should_enqueue_and_run_task() {
        let (runner, _dir) = test_runner(false).await;
        let task = runner
            .enqueue(EnqueueRequest {
                agent_id: "main".to_string(),
                message: "say hello".to_string(),
                kind: TaskKind::Cli,
                depends_on: Vec::new(),
            })
            .await
            .unwrap();

        let executed = runner.run(&task.id).await.unwrap();
        assert_eq!(executed.status, TaskStatus::Completed);
        assert!(executed.started_at.is_some());
    }

    #[tokio::test]
    async fn should_fail_task_on_runtime_error() {
        let (runner, _dir) = test_runner(true).await;
        let task = runner
            .enqueue(EnqueueRequest {
                agent_id: "main".to_string(),
                message: "fail".to_string(),
                kind: TaskKind::Cli,
                depends_on: Vec::new(),
            })
            .await
            .unwrap();

        let executed = runner.run(&task.id).await.unwrap();
        assert_eq!(executed.status, TaskStatus::Failed);
        assert!(executed.error.unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn should_process_pending_tasks() {
        let (runner, _dir) = test_runner(false).await;
        runner
            .enqueue(EnqueueRequest {
                agent_id: "main".to_string(),
                message: "first".to_string(),
                kind: TaskKind::Cli,
                depends_on: Vec::new(),
            })
            .await
            .unwrap();

        let results = runner.process_pending().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().status, TaskStatus::Completed);

        // No more pending tasks.
        let results = runner.process_pending().await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn should_respect_task_dependencies() {
        let (runner, _dir) = test_runner(false).await;
        let dep = runner
            .enqueue(EnqueueRequest {
                agent_id: "main".to_string(),
                message: "dep".to_string(),
                kind: TaskKind::Cli,
                depends_on: Vec::new(),
            })
            .await
            .unwrap();

        let follow_up = runner
            .enqueue(EnqueueRequest {
                agent_id: "main".to_string(),
                message: "follow up".to_string(),
                kind: TaskKind::Cli,
                depends_on: vec![dep.id.clone()],
            })
            .await
            .unwrap();

        // First pass: dep runs, follow-up waits.
        let results = runner.process_pending().await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().id, dep.id);
        assert!(matches!(
            results[1],
            Err(TaskRunnerError::DependenciesNotMet(_))
        ));

        // Second pass: follow-up runs.
        let results = runner.process_pending().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().id, follow_up.id);
        assert_eq!(results[0].as_ref().unwrap().status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn should_fail_task_with_missing_dependency() {
        let (runner, _dir) = test_runner(false).await;
        let task = runner
            .enqueue(EnqueueRequest {
                agent_id: "main".to_string(),
                message: "orphan".to_string(),
                kind: TaskKind::Cli,
                depends_on: vec!["does-not-exist".to_string()],
            })
            .await
            .unwrap();

        let results = runner.process_pending().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());

        let stored = runner.task_store.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Failed);
    }
}
