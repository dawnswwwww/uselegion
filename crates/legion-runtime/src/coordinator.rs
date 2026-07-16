//! Coordinator mode (multi-agent Phase C).
//!
//! Executes a declared multi-phase plan on top of the [`SubagentSpawner`]
//! seam: tasks within a phase run concurrently; phases run sequentially in
//! declaration order, each gated on its `depends_on` phases. Task prompts may
//! contain `{{results}}`, which is replaced with the accumulated results of
//! all previously completed phases (the synthesis pattern).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tracing::Instrument;

use crate::subagent::{SubagentKind, SubagentRequest, SubagentResult, SubagentSpawner};

/// A declared multi-phase plan (parsed from the `run_coordinator` tool input).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoordinatorPlan {
    pub phases: Vec<CoordinatorPhase>,
}

/// One phase of a [`CoordinatorPlan`]. Phases execute sequentially in
/// declaration order; a phase starts only after every phase named in
/// `depends_on` has completed.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoordinatorPhase {
    pub name: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub tasks: Vec<CoordinatorTask>,
}

/// A single sub-agent task inside a phase. Tasks are always
/// [`SubagentKind::Typed`] children of the run that invoked the plan.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoordinatorTask {
    /// Child agent type (an entry in `agents.list`, or `main`).
    pub agent_type: String,
    /// Task instruction; `{{results}}` is replaced with the accumulated
    /// results of all previously completed phases.
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Per-task tool subset. Must be within the invoking run's effective set
    /// (validated by the tool layer before execution).
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Outcome of one executed phase, in task declaration order.
#[derive(Debug, Clone)]
pub struct PhaseReport {
    pub name: String,
    pub results: Vec<SubagentResult>,
}

/// Accumulated outcome of a plan, one entry per phase.
#[derive(Debug, Clone, Default)]
pub struct CoordinatorReport {
    pub phases: Vec<PhaseReport>,
}

impl CoordinatorReport {
    /// Render every phase/task as a text block, used both for `{{results}}`
    /// injection and for the final tool output.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for phase in &self.phases {
            for (idx, res) in phase.results.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "[phase {} / task {} / {}]",
                    phase.name,
                    idx + 1,
                    res.status
                );
                out.push_str(&res.text);
                out.push('\n');
                if let Some(path) = &res.transcript_path {
                    let _ = writeln!(out, "(transcript: {})", path.display());
                }
            }
        }
        out
    }
}

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error("invalid plan: {0}")]
    Validation(String),
    #[error("failed to spawn task in phase '{phase}': {source}")]
    Spawn {
        phase: String,
        source: crate::subagent::SubagentError,
    },
    #[error("failed to join task in phase '{phase}': {source}")]
    Join {
        phase: String,
        source: crate::subagent::SubagentError,
    },
}

/// Execute a plan on the given spawner. Tasks of a phase are spawned
/// together (concurrent, bounded by the spawner's own semaphore) and joined
/// in declaration order; phases run one after another. `parent_agent_id` and
/// `parent_depth` describe the invoking run (each task is its direct child).
pub async fn run_coordinator_plan(
    plan: &CoordinatorPlan,
    spawner: &Arc<dyn SubagentSpawner>,
    parent_agent_id: &str,
    parent_depth: u8,
) -> Result<CoordinatorReport, CoordinatorError> {
    validate_plan(plan)?;

    let mut report = CoordinatorReport::default();
    for phase in &plan.phases {
        let accumulated = report.render();
        let span =
            tracing::info_span!("coordinator.phase", name = %phase.name, tasks = phase.tasks.len());

        let results = async {
            let mut handles = Vec::with_capacity(phase.tasks.len());
            for task in &phase.tasks {
                let prompt = task.prompt.replace("{{results}}", &accumulated);
                let req = SubagentRequest {
                    kind: SubagentKind::Typed(task.agent_type.clone()),
                    prompt,
                    model: task.model.clone(),
                    allowed_tools: task.allowed_tools.clone(),
                    parent_agent_id: parent_agent_id.to_string(),
                    parent_depth,
                    system_prompt: task.system_prompt.clone(),
                    history: Vec::new(),
                    max_iterations: task.max_iterations,
                    timeout: task.timeout_ms.map(Duration::from_millis),
                };
                let handle =
                    spawner
                        .spawn(req)
                        .await
                        .map_err(|source| CoordinatorError::Spawn {
                            phase: phase.name.clone(),
                            source,
                        })?;
                handles.push(handle);
            }
            let mut results = Vec::with_capacity(handles.len());
            for handle in handles {
                let result = handle
                    .join()
                    .await
                    .map_err(|source| CoordinatorError::Join {
                        phase: phase.name.clone(),
                        source,
                    })?;
                results.push(result);
            }
            Ok::<_, CoordinatorError>(results)
        }
        .instrument(span)
        .await?;

        report.phases.push(PhaseReport {
            name: phase.name.clone(),
            results,
        });
    }
    Ok(report)
}

/// Validate structural rules: unique non-empty phase names, non-empty task
/// lists, and `depends_on` pointing at phases declared earlier (which also
/// makes declaration order a valid topological order and rules out cycles).
fn validate_plan(plan: &CoordinatorPlan) -> Result<(), CoordinatorError> {
    if plan.phases.is_empty() {
        return Err(CoordinatorError::Validation(
            "plan must contain at least one phase".to_string(),
        ));
    }
    let mut declared: HashSet<&str> = HashSet::new();
    for phase in &plan.phases {
        if phase.name.trim().is_empty() {
            return Err(CoordinatorError::Validation(
                "phase name must not be empty".to_string(),
            ));
        }
        if !declared.insert(phase.name.as_str()) {
            return Err(CoordinatorError::Validation(format!(
                "duplicate phase name '{}'",
                phase.name
            )));
        }
        for dep in &phase.depends_on {
            if !declared.contains(dep.as_str()) {
                return Err(CoordinatorError::Validation(format!(
                    "phase '{}' depends on '{}' which is not declared before it",
                    phase.name, dep
                )));
            }
        }
        if phase.tasks.is_empty() {
            return Err(CoordinatorError::Validation(format!(
                "phase '{}' must contain at least one task",
                phase.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::{SubagentError, SubagentHandle, SubagentStatus};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tokio::sync::oneshot;

    /// Fake spawner that records every request in order and replies with a
    /// canned result derived from the task index.
    struct RecordingSpawner {
        requests: Arc<Mutex<Vec<SubagentRequest>>>,
    }

    impl RecordingSpawner {
        fn new() -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl SubagentSpawner for RecordingSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<SubagentHandle, SubagentError> {
            self.requests.lock().unwrap().push(req);
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(SubagentResult {
                handle_id: "h".into(),
                text: "task-output".into(),
                tool_call_count: 0,
                transcript_path: None,
                status: SubagentStatus::Completed,
            });
            Ok(SubagentHandle::from_receiver("h".into(), rx))
        }
    }

    fn plan_json() -> serde_json::Value {
        serde_json::json!({
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

    #[test]
    fn plan_deserializes_camel_case() {
        let plan: CoordinatorPlan = serde_json::from_value(plan_json()).expect("plan parses");
        assert_eq!(plan.phases.len(), 2);
        assert_eq!(plan.phases[1].depends_on, vec!["research".to_string()]);
        assert_eq!(plan.phases[0].tasks.len(), 2);
    }

    #[test]
    fn plan_rejects_unknown_fields() {
        let mut v = plan_json();
        v["phases"][0]["bogus"] = serde_json::json!(1);
        assert!(serde_json::from_value::<CoordinatorPlan>(v).is_err());
    }

    #[test]
    fn validation_rejects_forward_dependency() {
        let v = serde_json::json!({
            "phases": [
                { "name": "a", "dependsOn": ["b"], "tasks": [{ "agentType": "main", "prompt": "x" }] },
                { "name": "b", "tasks": [{ "agentType": "main", "prompt": "y" }] }
            ]
        });
        let plan: CoordinatorPlan = serde_json::from_value(v).unwrap();
        let err = validate_plan(&plan).expect_err("forward dep must fail");
        assert!(err.to_string().contains("not declared before"));
    }

    #[test]
    fn validation_rejects_duplicate_phase_names() {
        let v = serde_json::json!({
            "phases": [
                { "name": "a", "tasks": [{ "agentType": "main", "prompt": "x" }] },
                { "name": "a", "tasks": [{ "agentType": "main", "prompt": "y" }] }
            ]
        });
        let plan: CoordinatorPlan = serde_json::from_value(v).unwrap();
        let err = validate_plan(&plan).expect_err("duplicate names must fail");
        assert!(err.to_string().contains("duplicate phase name"));
    }

    #[test]
    fn validation_rejects_empty_tasks() {
        let v = serde_json::json!({
            "phases": [ { "name": "a", "tasks": [] } ]
        });
        let plan: CoordinatorPlan = serde_json::from_value(v).unwrap();
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn validation_rejects_empty_plan() {
        let v = serde_json::json!({ "phases": [] });
        let plan: CoordinatorPlan = serde_json::from_value(v).unwrap();
        let err = validate_plan(&plan).expect_err("empty plan must fail");
        assert!(err.to_string().contains("at least one phase"));
    }

    #[test]
    fn validation_rejects_blank_phase_name() {
        let v = serde_json::json!({
            "phases": [ { "name": "  ", "tasks": [{ "agentType": "main", "prompt": "x" }] } ]
        });
        let plan: CoordinatorPlan = serde_json::from_value(v).unwrap();
        let err = validate_plan(&plan).expect_err("blank phase name must fail");
        assert!(err.to_string().contains("phase name must not be empty"));
    }

    /// Fake spawner that fails every spawn with a canned error.
    struct FailingSpawner;

    #[async_trait]
    impl SubagentSpawner for FailingSpawner {
        async fn spawn(&self, _req: SubagentRequest) -> Result<SubagentHandle, SubagentError> {
            Err(SubagentError::Validation("spawner offline".into()))
        }
    }

    #[tokio::test]
    async fn coordinator_propagates_spawn_failure() {
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(FailingSpawner);
        let plan: CoordinatorPlan = serde_json::from_value(plan_json()).unwrap();

        let err = run_coordinator_plan(&plan, &spawner, "main", 0)
            .await
            .expect_err("spawn failure must abort the plan");
        match err {
            CoordinatorError::Spawn { phase, source } => {
                assert_eq!(phase, "research", "error must name the failing phase");
                assert!(
                    source.to_string().contains("spawner offline"),
                    "source error must be preserved, got {source}"
                );
            }
            other => panic!("expected CoordinatorError::Spawn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_runs_phase_tasks_concurrently_then_serializes_phases() {
        let recording = Arc::new(RecordingSpawner::new());
        let spawner: Arc<dyn SubagentSpawner> = recording.clone();
        let plan: CoordinatorPlan = serde_json::from_value(plan_json()).unwrap();

        let report = run_coordinator_plan(&plan, &spawner, "main", 0)
            .await
            .expect("plan runs");

        assert_eq!(report.phases.len(), 2);
        assert_eq!(report.phases[0].results.len(), 2);
        assert_eq!(report.phases[1].results.len(), 1);

        let reqs = recording.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        // Same-phase tasks are both spawned before any join completes; the
        // synthesis task is spawned only after the research results exist.
        assert_eq!(reqs[0].prompt, "gather facts");
        assert_eq!(reqs[1].prompt, "gather more");
        assert!(
            reqs[2].prompt.contains("task-output"),
            "synthesis prompt must include accumulated results, got {:?}",
            reqs[2].prompt
        );
        assert!(
            reqs[2]
                .prompt
                .contains("[phase research / task 1 / completed]"),
            "got {:?}",
            reqs[2].prompt
        );
        // Tasks are direct children of the invoking run.
        assert!(
            reqs.iter()
                .all(|r| r.parent_depth == 0 && r.parent_agent_id == "main")
        );
    }
}
