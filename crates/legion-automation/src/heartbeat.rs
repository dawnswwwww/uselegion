//! Periodic heartbeat that triggers a main-session agent turn for batched checks.
//!
//! The heartbeat reads `HEARTBEAT.md` from the workspace as a checklist, runs a
//! single agent turn with system prompt guidance, and explicitly does NOT create
//! task records or extend session idle freshness.

use futures::StreamExt;
use legion_runtime::{Harness, RunRequest};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Configuration for the heartbeat service.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    pub agent_id: String,
    pub interval_minutes: u32,
    pub workspace: PathBuf,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            agent_id: "main".to_string(),
            interval_minutes: 30,
            workspace: crate::home_workspace(),
        }
    }
}

/// Heartbeat service.
pub struct Heartbeat {
    config: HeartbeatConfig,
    runtime: Arc<dyn Harness>,
    model_ref: String,
}

impl Heartbeat {
    pub fn new(
        config: HeartbeatConfig,
        runtime: Arc<dyn Harness>,
        model_ref: impl Into<String>,
    ) -> Self {
        Self {
            config,
            runtime,
            model_ref: model_ref.into(),
        }
    }

    /// Assemble the heartbeat prompt from `HEARTBEAT.md` if present.
    fn assemble_prompt(&self) -> String {
        let checklist_path = self.config.workspace.join("HEARTBEAT.md");
        let checklist = std::fs::read_to_string(&checklist_path).unwrap_or_default();
        let mut prompt = String::from(
            "This is a periodic heartbeat check. Review the checklist below and take any needed actions in a single turn. Do not create task records.\n\n",
        );
        if checklist.trim().is_empty() {
            prompt.push_str("(no HEARTBEAT.md checklist found)\n");
        } else {
            prompt.push_str(&checklist);
        }
        prompt
    }

    /// Run a single heartbeat turn synchronously (blocking the heartbeat loop).
    pub async fn tick(&self) {
        let prompt = self.assemble_prompt();
        let request = RunRequest::new(
            session_key_for_heartbeat(&self.config.agent_id),
            &self.config.agent_id,
            "Run the heartbeat checklist.",
            &self.model_ref,
        )
        .with_system_prompt(prompt);

        let mut stream = match self.runtime.run(request) {
            Ok(stream) => stream,
            Err(err) => {
                warn!(error = %err, "heartbeat failed to start agent run");
                return;
            }
        };

        // Drain the stream without recording tasks or touching session idle state.
        while stream.next().await.is_some() {}
        info!("heartbeat turn completed");
    }

    /// Start the background heartbeat loop.
    pub async fn run(self: Arc<Self>) {
        let interval = Duration::from_secs(u64::from(self.config.interval_minutes) * 60);
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            self.tick().await;
        }
    }
}

fn session_key_for_heartbeat(agent_id: &str) -> String {
    format!(
        "agent:{}:heartbeat:heartbeat:default:direct:heartbeat",
        agent_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::{LifecyclePhase, RunEvent, RunStream, RuntimeError};

    struct CountingHarness {
        calls: std::sync::Mutex<usize>,
        notify: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Harness for CountingHarness {
        fn id(&self) -> &str {
            "counting"
        }

        fn can_handle(&self, _model_ref: &str) -> bool {
            true
        }

        fn run(&self, _request: RunRequest) -> Result<RunStream, RuntimeError> {
            *self.calls.lock().unwrap() += 1;
            self.notify.notify_one();
            Ok(Box::pin(futures::stream::iter(vec![RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                error: None,
            }])))
        }
    }

    struct FailingHarness;

    #[async_trait::async_trait]
    impl Harness for FailingHarness {
        fn id(&self) -> &str {
            "failing"
        }

        fn can_handle(&self, _model_ref: &str) -> bool {
            true
        }

        fn run(&self, _request: RunRequest) -> Result<RunStream, RuntimeError> {
            Err(RuntimeError::Context("boom".to_string()))
        }
    }

    fn test_heartbeat(harness: Arc<dyn Harness>, workspace: PathBuf) -> Heartbeat {
        let config = HeartbeatConfig {
            agent_id: "main".to_string(),
            interval_minutes: 1,
            workspace,
        };
        Heartbeat::new(config, harness, "text/gpt")
    }

    #[tokio::test]
    async fn should_run_heartbeat_on_interval() {
        tokio::time::pause();

        let harness = Arc::new(CountingHarness {
            calls: std::sync::Mutex::new(0),
            notify: tokio::sync::Notify::new(),
        });
        let heartbeat = Arc::new(test_heartbeat(
            harness.clone(),
            tempfile::tempdir().unwrap().path().to_path_buf(),
        ));

        // Spawn the heartbeat loop and advance past the first interval.
        let handle = tokio::spawn(heartbeat.run());
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();

        // Wait for the harness to actually observe a run (no fixed sleep);
        // the timeout only fires if the heartbeat loop regresses.
        tokio::time::timeout(Duration::from_secs(5), harness.notify.notified())
            .await
            .expect("heartbeat loop did not invoke the harness");
        handle.abort();

        let calls = *harness.calls.lock().unwrap();
        assert!(calls >= 1, "expected at least one heartbeat, got {calls}");
    }

    #[test]
    fn should_use_default_interval() {
        let config = HeartbeatConfig::default();
        assert_eq!(config.interval_minutes, 30);
    }

    #[test]
    fn heartbeat_prompt_includes_checklist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("HEARTBEAT.md"),
            "- Check cron health\n- Summarize pending tasks\n",
        )
        .unwrap();
        let harness = Arc::new(CountingHarness {
            calls: std::sync::Mutex::new(0),
            notify: tokio::sync::Notify::new(),
        });
        let heartbeat = test_heartbeat(harness, dir.path().to_path_buf());

        let prompt = heartbeat.assemble_prompt();

        assert!(prompt.contains("Check cron health"));
        assert!(prompt.contains("Summarize pending tasks"));
        assert!(!prompt.contains("(no HEARTBEAT.md checklist found)"));
    }

    #[test]
    fn heartbeat_prompt_notes_missing_checklist() {
        let dir = tempfile::tempdir().unwrap();
        let harness = Arc::new(CountingHarness {
            calls: std::sync::Mutex::new(0),
            notify: tokio::sync::Notify::new(),
        });
        let heartbeat = test_heartbeat(harness, dir.path().to_path_buf());

        let prompt = heartbeat.assemble_prompt();

        assert!(prompt.contains("(no HEARTBEAT.md checklist found)"));
    }

    #[tokio::test]
    async fn heartbeat_tick_swallows_runtime_error() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = test_heartbeat(Arc::new(FailingHarness), dir.path().to_path_buf());

        // tick() must log and return normally when the runtime fails to start.
        heartbeat.tick().await;
    }
}
