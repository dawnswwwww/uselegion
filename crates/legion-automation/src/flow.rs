//! Task Flow DAG runner (automation-advanced Phase C).
//!
//! A [`TaskFlow`] declared in config is a named DAG of agent steps. The runner
//! executes ready steps layer by layer — steps whose dependencies all
//! completed run concurrently — and applies the flow's failure policy when a
//! step fails. Conditional branches and revision loops are Phase D.

use chrono::{DateTime, Utc};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use legion_core::config::{Config, FlowFailurePolicy, FlowStep, TaskFlow};
use legion_runtime::{Harness, LifecyclePhase, RunEvent, RunRequest};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::info;

/// Overall outcome of a flow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FlowStatus {
    Completed,
    Failed,
}

/// Outcome of a single flow step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepStatus {
    Completed,
    Failed,
    Skipped,
}

/// Per-step outcome in a [`FlowReport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepOutcome {
    pub name: String,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Serializable report of a flow run, returned over the gateway RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowReport {
    pub flow_id: String,
    pub status: FlowStatus,
    pub steps: Vec<StepOutcome>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// Executes declarative [`TaskFlow`]s against an agent harness.
pub struct FlowRunner {
    pub runtime: Arc<dyn Harness>,
    pub config: Config,
}

impl FlowRunner {
    pub fn new(runtime: Arc<dyn Harness>, config: Config) -> Self {
        Self { runtime, config }
    }

    /// Run a flow to completion and return a report of every step outcome.
    /// The report's `steps` preserve the flow's declaration order.
    pub async fn run_flow(&self, flow: &TaskFlow) -> FlowReport {
        let started_at = Utc::now();
        info!(flow_id = %flow.id, steps = flow.steps.len(), "task flow started");

        // Status per step name; absent means "not started yet".
        let mut statuses: HashMap<&str, StepStatus> = HashMap::new();
        let mut errors: HashMap<&str, String> = HashMap::new();

        // Pre-validation: duplicate step names or unknown dependency references
        // fail the whole flow before any step executes.
        let mut failed = false;
        if let Some(reason) = validate_flow(flow) {
            info!(flow_id = %flow.id, %reason, "task flow rejected at validation");
            for step in &flow.steps {
                statuses.insert(step.name.as_str(), StepStatus::Skipped);
            }
            failed = true;
        }

        let mut aborted = false;
        while !failed && !aborted {
            let ready: Vec<&FlowStep> = flow
                .steps
                .iter()
                .filter(|s| !statuses.contains_key(s.name.as_str()))
                .filter(|s| {
                    s.depends_on
                        .iter()
                        .all(|d| statuses.get(d.as_str()) == Some(&StepStatus::Completed))
                })
                .collect();

            if ready.is_empty() {
                // No ready step but unfinished steps remain: dependency cycle.
                let remaining: Vec<&str> = flow
                    .steps
                    .iter()
                    .filter(|s| !statuses.contains_key(s.name.as_str()))
                    .map(|s| s.name.as_str())
                    .collect();
                if remaining.is_empty() {
                    break;
                }
                info!(flow_id = %flow.id, ?remaining, "task flow has unresolvable dependencies (cycle)");
                for name in remaining {
                    statuses.insert(name, StepStatus::Skipped);
                }
                failed = true;
                break;
            }

            let layer: Vec<&str> = ready.iter().map(|s| s.name.as_str()).collect();
            info!(flow_id = %flow.id, ?layer, "task flow executing step layer");

            let mut in_flight: FuturesUnordered<_> =
                ready.iter().map(|step| self.run_step(flow, step)).collect();

            while let Some((name, result)) = in_flight.next().await {
                match result {
                    Ok(()) => {
                        statuses.insert(name, StepStatus::Completed);
                    }
                    Err(error) => {
                        info!(flow_id = %flow.id, step = %name, %error, "task flow step failed");
                        statuses.insert(name, StepStatus::Failed);
                        errors.insert(name, error);
                        failed = true;
                        match flow.on_failure {
                            FlowFailurePolicy::Abort => {
                                for step in &flow.steps {
                                    statuses
                                        .entry(step.name.as_str())
                                        .or_insert(StepStatus::Skipped);
                                }
                                aborted = true;
                            }
                            FlowFailurePolicy::Continue => {
                                for dependent in transitive_dependents(&flow.steps, name) {
                                    statuses.entry(dependent).or_insert(StepStatus::Skipped);
                                }
                            }
                        }
                    }
                }
            }
        }

        let ended_at = Utc::now();
        let steps = flow
            .steps
            .iter()
            .map(|s| StepOutcome {
                name: s.name.clone(),
                status: statuses
                    .get(s.name.as_str())
                    .copied()
                    .unwrap_or(StepStatus::Completed),
                error: errors.get(s.name.as_str()).cloned(),
            })
            .collect();
        let status = if failed {
            FlowStatus::Failed
        } else {
            FlowStatus::Completed
        };
        info!(flow_id = %flow.id, ?status, "task flow finished");
        FlowReport {
            flow_id: flow.id.clone(),
            status,
            steps,
            started_at,
            ended_at,
        }
    }

    async fn run_step<'a>(
        &self,
        flow: &'a TaskFlow,
        step: &'a FlowStep,
    ) -> (&'a str, Result<(), String>) {
        let model_ref = resolve_model(&self.config, &flow.agent_id);
        let session_id = format!("agent:{}:flow:{}:{}", flow.agent_id, flow.id, step.name);
        let request = RunRequest::new(&session_id, &flow.agent_id, &step.message, model_ref)
            .with_system_prompt(format!(
                "You are executing step '{}' of task flow '{}'. Complete the following instruction:",
                step.name, flow.id
            ));

        let result = match self.runtime.run(request) {
            Ok(mut stream) => {
                let mut saw_error = None;
                while let Some(event) = stream.next().await {
                    if let RunEvent::Lifecycle {
                        phase: LifecyclePhase::Error,
                        error,
                    } = event
                    {
                        saw_error = Some(error.unwrap_or_else(|| "flow step failed".to_string()));
                        break;
                    }
                }
                match saw_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }
            Err(err) => Err(err.to_string()),
        };
        (step.name.as_str(), result)
    }
}

/// Validate a flow before execution: step names must be unique and every
/// `depends_on` entry must reference an existing step. Returns a reason on
/// failure.
fn validate_flow(flow: &TaskFlow) -> Option<String> {
    let mut names = HashSet::new();
    for step in &flow.steps {
        if !names.insert(step.name.as_str()) {
            return Some(format!("duplicate step name '{}'", step.name));
        }
    }
    for step in &flow.steps {
        for dep in &step.depends_on {
            if !names.contains(dep.as_str()) {
                return Some(format!(
                    "step '{}' depends on unknown step '{}'",
                    step.name, dep
                ));
            }
        }
    }
    None
}

/// Return the names of all steps that transitively depend on `failed`
/// (excluding `failed` itself). Pure function for the `Continue` policy.
pub fn transitive_dependents<'a>(steps: &'a [FlowStep], failed: &str) -> HashSet<&'a str> {
    let mut skipped: HashSet<&str> = HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for step in steps {
            if step.name == failed || skipped.contains(step.name.as_str()) {
                continue;
            }
            let depends_on_failed = step
                .depends_on
                .iter()
                .any(|d| d == failed || skipped.contains(d.as_str()));
            if depends_on_failed {
                skipped.insert(step.name.as_str());
                changed = true;
            }
        }
    }
    skipped
}

fn resolve_model(config: &Config, agent_id: &str) -> String {
    if agent_id == "main" {
        config.agents.defaults.model.clone()
    } else {
        config
            .agents
            .list
            .iter()
            .find(|a| a.id == agent_id)
            .and_then(|a| a.model.clone())
            .or_else(|| config.agents.defaults.model.clone())
    }
    .unwrap_or_else(|| "openai/gpt-4o".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::{RunStream, RuntimeError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Tracks how many runs were started and the peak concurrent run count.
    #[derive(Default)]
    struct Tracker {
        runs: AtomicUsize,
        current: AtomicUsize,
        max: AtomicUsize,
    }

    /// Decrements the in-flight counter when the stream is dropped or ends.
    struct RunGuard(Arc<Tracker>);

    impl Drop for RunGuard {
        fn drop(&mut self) {
            self.0.current.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Fails any run whose message contains "FAIL"; records concurrency so
    /// tests can assert that a dependency layer actually ran in parallel.
    struct FakeHarness {
        tracker: Arc<Tracker>,
    }

    #[async_trait::async_trait]
    impl Harness for FakeHarness {
        fn id(&self) -> &str {
            "fake"
        }

        fn can_handle(&self, _model_ref: &str) -> bool {
            true
        }

        fn run(&self, request: RunRequest) -> Result<RunStream, RuntimeError> {
            self.tracker.runs.fetch_add(1, Ordering::SeqCst);
            if request.user_message.contains("FAIL") {
                return Ok(Box::pin(futures::stream::iter(vec![RunEvent::Lifecycle {
                    phase: LifecyclePhase::Error,
                    error: Some("boom".to_string()),
                }])));
            }
            let current = self.tracker.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.tracker.max.fetch_max(current, Ordering::SeqCst);
            let guard = RunGuard(self.tracker.clone());
            Ok(Box::pin(futures::stream::unfold(
                (0usize, guard),
                |(i, guard)| async move {
                    let events = [
                        RunEvent::Lifecycle {
                            phase: LifecyclePhase::Start,
                            error: None,
                        },
                        RunEvent::Lifecycle {
                            phase: LifecyclePhase::End,
                            error: None,
                        },
                    ];
                    if i >= events.len() {
                        None
                    } else {
                        // Keep the run alive briefly so concurrent steps in
                        // the same layer overlap in time.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Some((events[i].clone(), (i + 1, guard)))
                    }
                },
            )))
        }
    }

    fn test_runner() -> (FlowRunner, Arc<Tracker>) {
        let tracker = Arc::new(Tracker::default());
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let runner = FlowRunner::new(
            Arc::new(FakeHarness {
                tracker: tracker.clone(),
            }),
            config,
        );
        (runner, tracker)
    }

    fn step(name: &str, message: &str, depends_on: &[&str]) -> FlowStep {
        FlowStep {
            name: name.to_string(),
            message: message.to_string(),
            depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn flow(steps: Vec<FlowStep>, on_failure: FlowFailurePolicy) -> TaskFlow {
        TaskFlow {
            id: "f1".to_string(),
            agent_id: "main".to_string(),
            steps,
            on_failure,
        }
    }

    fn outcome<'a>(report: &'a FlowReport, name: &str) -> &'a StepOutcome {
        report
            .steps
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("missing step outcome for {name}"))
    }

    #[tokio::test]
    async fn linear_flow_completes_all_steps() {
        let (runner, _tracker) = test_runner();
        let f = flow(
            vec![
                step("a", "first", &[]),
                step("b", "second", &["a"]),
                step("c", "third", &["b"]),
            ],
            FlowFailurePolicy::Abort,
        );

        let report = runner.run_flow(&f).await;

        assert_eq!(report.status, FlowStatus::Completed);
        for name in ["a", "b", "c"] {
            assert_eq!(outcome(&report, name).status, StepStatus::Completed);
        }
        assert!(report.ended_at >= report.started_at);
    }

    #[tokio::test]
    async fn diamond_flow_runs_layer_concurrently() {
        let (runner, tracker) = test_runner();
        let f = flow(
            vec![
                step("a", "root", &[]),
                step("b", "left", &["a"]),
                step("c", "right", &["a"]),
                step("d", "join", &["b", "c"]),
            ],
            FlowFailurePolicy::Abort,
        );

        let report = runner.run_flow(&f).await;

        assert_eq!(report.status, FlowStatus::Completed);
        for name in ["a", "b", "c", "d"] {
            assert_eq!(outcome(&report, name).status, StepStatus::Completed);
        }
        assert_eq!(tracker.runs.load(Ordering::SeqCst), 4);
        // b and c share a layer; they must have overlapped.
        assert!(tracker.max.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn abort_policy_skips_everything_after_failure() {
        let (runner, _tracker) = test_runner();
        let f = flow(
            vec![
                step("a", "first", &[]),
                step("b", "FAIL here", &["a"]),
                step("c", "third", &["b"]),
                step("d", "fourth", &["c"]),
            ],
            FlowFailurePolicy::Abort,
        );

        let report = runner.run_flow(&f).await;

        assert_eq!(report.status, FlowStatus::Failed);
        assert_eq!(outcome(&report, "a").status, StepStatus::Completed);
        assert_eq!(outcome(&report, "b").status, StepStatus::Failed);
        assert!(outcome(&report, "b").error.is_some());
        assert_eq!(outcome(&report, "c").status, StepStatus::Skipped);
        assert_eq!(outcome(&report, "d").status, StepStatus::Skipped);
    }

    #[tokio::test]
    async fn continue_policy_skips_only_dependents() {
        let (runner, _tracker) = test_runner();
        let f = flow(
            vec![
                step("a", "root", &[]),
                step("b", "FAIL branch", &["a"]),
                step("c", "downstream", &["b"]),
                step("d", "sibling", &["a"]),
            ],
            FlowFailurePolicy::Continue,
        );

        let report = runner.run_flow(&f).await;

        assert_eq!(report.status, FlowStatus::Failed);
        assert_eq!(outcome(&report, "a").status, StepStatus::Completed);
        assert_eq!(outcome(&report, "b").status, StepStatus::Failed);
        assert_eq!(outcome(&report, "c").status, StepStatus::Skipped);
        assert_eq!(outcome(&report, "d").status, StepStatus::Completed);
    }

    #[tokio::test]
    async fn cyclic_flow_fails_without_executing() {
        let (runner, tracker) = test_runner();
        let f = flow(
            vec![step("a", "first", &["b"]), step("b", "second", &["a"])],
            FlowFailurePolicy::Abort,
        );

        let report = runner.run_flow(&f).await;

        assert_eq!(report.status, FlowStatus::Failed);
        assert_eq!(outcome(&report, "a").status, StepStatus::Skipped);
        assert_eq!(outcome(&report, "b").status, StepStatus::Skipped);
        assert_eq!(tracker.runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_dependency_fails_without_executing() {
        let (runner, tracker) = test_runner();
        let f = flow(
            vec![step("a", "first", &["nope"])],
            FlowFailurePolicy::Abort,
        );

        let report = runner.run_flow(&f).await;

        assert_eq!(report.status, FlowStatus::Failed);
        assert_eq!(outcome(&report, "a").status, StepStatus::Skipped);
        assert_eq!(tracker.runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn duplicate_step_names_fail_without_executing() {
        let (runner, tracker) = test_runner();
        let f = flow(
            vec![step("a", "one", &[]), step("a", "two", &[])],
            FlowFailurePolicy::Abort,
        );

        let report = runner.run_flow(&f).await;

        assert_eq!(report.status, FlowStatus::Failed);
        assert_eq!(tracker.runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn transitive_dependents_walks_the_whole_chain() {
        let steps = vec![
            step("a", "", &[]),
            step("b", "", &["a"]),
            step("c", "", &["b"]),
            step("d", "", &["a"]),
            step("e", "", &["c"]),
            step("unrelated", "", &[]),
        ];
        let deps = transitive_dependents(&steps, "a");
        let expected: HashSet<&str> = ["b", "c", "d", "e"].into_iter().collect();
        assert_eq!(deps, expected);
        assert!(!deps.contains("a"));
        assert!(!deps.contains("unrelated"));
    }
}
