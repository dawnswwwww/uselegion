//! Cron scheduler for recurring and one-shot agent runs.
//!
//! Jobs are persisted to a JSONL file and each execution creates a task record
//! and triggers an isolated agent session.

use crate::tasks::{SharedTaskStore, Task, TaskKind};
use chrono::{DateTime, Duration as ChronoDuration, Local, TimeZone, Utc};
use cron::Schedule;
use futures::StreamExt;
use legion_provider::model_ref::resolve_agent_model;
use legion_runtime::{ApprovalGate, Harness, RunRequest};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, OnceLock, Weak};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Errors that can occur in the cron subsystem.
#[derive(Debug, Error)]
pub enum CronError {
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),
    #[error("job '{0}' not found")]
    NotFound(String),
    #[error("job id '{0}' already exists")]
    DuplicateId(String),
    #[error("job '{0}' is already running")]
    AlreadyRunning(String),
    #[error("task store error: {0}")]
    TaskStore(#[from] crate::tasks::TaskStoreError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("runtime error: {0}")]
    Runtime(String),
}

/// A persisted cron job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub agent_id: String,
    pub message: String,
    /// Human-readable name for the job, introduced by the `scheduler_create`
    /// tool. Older records may leave this empty.
    #[serde(default)]
    pub name: String,
    /// Either a cron expression or the sentinel value `"__at__"` for one-shot jobs.
    pub schedule: String,
    /// For one-shot jobs, the scheduled run time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    /// HMAC-SHA256 secret that allows this job to be triggered via
    /// `POST /webhook/{id}` (automation-advanced Phase C). A job with a secret
    /// and the `"__webhook__"` schedule never fires on the clock — it runs only
    /// when a correctly signed webhook request arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
}

impl CronJob {
    /// Compute the next scheduled run time for this job.
    ///
    /// Cron fields are interpreted in the **local** timezone (standard cron
    /// semantics): a schedule like `53 18 16 7 *` fires at 18:53 local time,
    /// not 18:53 UTC. The returned instant is always UTC.
    pub fn compute_next_run(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.schedule == "__at__" {
            return self.at.filter(|t| *t > after);
        }
        match normalize_cron_expression(&self.schedule) {
            Ok(normalized) => Schedule::from_str(&normalized).ok().and_then(|schedule| {
                schedule
                    .after(&after.with_timezone(&Local))
                    .next()
                    .map(|next| next.with_timezone(&Utc))
            }),
            Err(_) => None,
        }
    }

    /// Recompute and update `next_run` based on the current time.
    pub fn refresh_next_run(&mut self) {
        self.next_run = self.compute_next_run(Utc::now());
    }

    /// Return true if this job should be considered a one-shot `--at` job.
    pub fn is_one_shot(&self) -> bool {
        self.schedule == "__at__"
    }
}

/// Store for cron job definitions.
#[async_trait::async_trait]
pub trait CronJobStore: Send + Sync {
    async fn create(&self, job: CronJob) -> Result<(), CronError>;
    async fn update(&self, job: CronJob) -> Result<(), CronError>;
    async fn remove(&self, id: &str) -> Result<(), CronError>;
    async fn list(&self) -> Result<Vec<CronJob>, CronError>;
    async fn get(&self, id: &str) -> Result<Option<CronJob>, CronError>;
}

type JobMap = HashMap<String, CronJob>;
type SharedJobMap = Arc<Mutex<JobMap>>;
type SharedJobMapWeak = Weak<Mutex<JobMap>>;
type PathSharedMap = HashMap<PathBuf, SharedJobMapWeak>;

/// In-memory job cache shared across all [`JsonlCronJobStore`] instances that
/// point to the same file path. This lets the cron scheduler and the
/// `scheduler_*` tools (which each construct their own store handle) see each
/// other's writes without having to reload the JSONL file on every tick.
static STORE_CACHE: OnceLock<std::sync::Mutex<PathSharedMap>> = OnceLock::new();

fn store_cache() -> &'static std::sync::Mutex<PathSharedMap> {
    STORE_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// JSONL-backed cron job store.
pub struct JsonlCronJobStore {
    path: PathBuf,
    jobs: SharedJobMap,
}

impl JsonlCronJobStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, CronError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Reuse the in-memory map for this path if one is still alive. This
        // keeps the scheduler loop and the agent tools in sync even when they
        // open separate store handles at the same path.
        {
            let mut cache = store_cache().lock().unwrap();
            if let Some(weak) = cache.get(&path) {
                if let Some(arc) = weak.upgrade() {
                    return Ok(Self { path, jobs: arc });
                }
                // The old handle was dropped; clean up the stale entry.
                cache.remove(&path);
            }
        }

        let jobs = Self::load(&path).await?;
        let jobs = Arc::new(Mutex::new(jobs));
        {
            let mut cache = store_cache().lock().unwrap();
            cache.insert(path.clone(), Arc::downgrade(&jobs));
        }
        Ok(Self { path, jobs })
    }

    async fn load(path: &Path) -> Result<HashMap<String, CronJob>, CronError> {
        let mut jobs = HashMap::new();
        if !path.exists() {
            return Ok(jobs);
        }
        let file = tokio::fs::File::open(path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<CronJob>(&line) {
                Ok(job) => {
                    jobs.insert(job.id.clone(), job);
                }
                Err(err) => {
                    tracing::warn!(line = %line, error = %err, "skipping malformed cron job")
                }
            }
        }
        Ok(jobs)
    }

    async fn save(&self, jobs: &HashMap<String, CronJob>) -> Result<(), CronError> {
        let mut buf = Vec::new();
        for job in jobs.values() {
            let line = serde_json::to_string(job)?;
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
        legion_core::fs::atomic_write_async(&self.path, &buf).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl CronJobStore for JsonlCronJobStore {
    async fn create(&self, job: CronJob) -> Result<(), CronError> {
        let mut jobs = self.jobs.lock().await;
        if jobs.contains_key(&job.id) {
            return Err(CronError::DuplicateId(job.id.clone()));
        }
        jobs.insert(job.id.clone(), job);
        self.save(&jobs).await
    }

    async fn update(&self, job: CronJob) -> Result<(), CronError> {
        let mut jobs = self.jobs.lock().await;
        if !jobs.contains_key(&job.id) {
            return Err(CronError::NotFound(job.id.clone()));
        }
        jobs.insert(job.id.clone(), job);
        self.save(&jobs).await
    }

    async fn remove(&self, id: &str) -> Result<(), CronError> {
        let mut jobs = self.jobs.lock().await;
        if jobs.remove(id).is_none() {
            return Err(CronError::NotFound(id.to_string()));
        }
        self.save(&jobs).await
    }

    async fn list(&self) -> Result<Vec<CronJob>, CronError> {
        let jobs = self.jobs.lock().await;
        Ok(jobs.values().cloned().collect())
    }

    async fn get(&self, id: &str) -> Result<Option<CronJob>, CronError> {
        let jobs = self.jobs.lock().await;
        Ok(jobs.get(id).cloned())
    }
}

/// A thread-safe boxed cron job store.
pub type SharedCronJobStore = Arc<dyn CronJobStore>;

/// Sink for cron job run events. The scheduler invokes this closure for every
/// event emitted while driving a scheduled run, allowing observers (e.g. the
/// TUI) to render progress without blocking the scheduler. The first argument
/// is the run id (the cron task id) so the observer can shape payloads exactly
/// like regular agent runs.
pub type CronEventSink = Arc<dyn Fn(&str, legion_runtime::RunEvent) + Send + Sync>;

/// Builder for adding a new cron job.
#[derive(Debug, Clone)]
pub struct AddJobRequest {
    pub schedule: String,
    pub agent_id: String,
    pub message: String,
    pub at: Option<DateTime<Utc>>,
    /// Human-readable job name. Defaults to an empty string if not set.
    pub name: String,
    /// Whether the job is enabled. Defaults to `true`.
    pub enabled: bool,
    /// When set together with the `"__webhook__"` schedule, creates a
    /// webhook-only job that is triggered via `POST /webhook/{id}` and skips
    /// cron-expression validation.
    pub webhook_secret: Option<String>,
    /// Optional ID prefix. When `Some`, the job id becomes `{prefix}-{ts}-{n}`,
    /// otherwise it defaults to `cron-{ts}-{n}`. Used to distinguish jobs that
    /// belong to different scopes (e.g. session-level vs. global gateway loops).
    pub id_prefix: Option<String>,
}

impl Default for AddJobRequest {
    fn default() -> Self {
        Self {
            schedule: String::new(),
            agent_id: String::new(),
            message: String::new(),
            at: None,
            name: String::new(),
            enabled: true,
            webhook_secret: None,
            id_prefix: None,
        }
    }
}

/// Scheduler that owns cron jobs, dispatches executions, and records tasks.
pub struct CronScheduler {
    job_store: SharedCronJobStore,
    task_store: SharedTaskStore,
    runtime: Arc<dyn Harness>,
    config: legion_core::config::Config,
    /// Optional observer for run events produced by scheduled jobs. Used by
    /// the local TUI to render cron output in the current window.
    event_sink: Option<CronEventSink>,
    /// Optional approval gate attached to every scheduled run. The local TUI
    /// uses this to propagate `--yolo` into session loops so unattended cron
    /// jobs can auto-approve tool prompts instead of waiting for a human who
    /// is not present.
    approval_gate: Option<Arc<ApprovalGate>>,
    /// Job ids currently being executed. Prevents the same job from being
    /// triggered twice concurrently by overlapping ticks or by a manual `run`
    /// while a scheduled tick is already in progress.
    in_flight: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
}

/// Create a cron job in `store` from `req`, applying the same validation,
/// id generation, and `next_run` calculation regardless of who calls it.
pub async fn create_job(
    store: &dyn CronJobStore,
    req: AddJobRequest,
) -> Result<CronJob, CronError> {
    let is_webhook_only = req.webhook_secret.is_some() && req.schedule == "__webhook__";
    let schedule = if let Some(at) = req.at {
        if req.schedule != "__at__" {
            return Err(CronError::InvalidExpression(
                "one-shot job must use __at__ schedule".to_string(),
            ));
        }
        validate_at(at)?;
        "__at__".to_string()
    } else if is_webhook_only {
        // Webhook-only job: no clock schedule, so no cron validation.
        "__webhook__".to_string()
    } else {
        validate_cron(&req.schedule)?;
        req.schedule.clone()
    };

    let prefix = req.id_prefix.as_deref().unwrap_or("cron");
    let id = crate::generate_job_id(prefix);
    let mut job = CronJob {
        id,
        agent_id: req.agent_id,
        message: req.message,
        name: req.name,
        schedule,
        at: req.at,
        enabled: req.enabled,
        created_at: Utc::now(),
        next_run: None,
        last_run: None,
        webhook_secret: req.webhook_secret,
    };
    job.refresh_next_run();
    store.create(job.clone()).await?;
    Ok(job)
}

impl CronScheduler {
    pub fn new(
        job_store: SharedCronJobStore,
        task_store: SharedTaskStore,
        runtime: Arc<dyn Harness>,
        config: legion_core::config::Config,
    ) -> Self {
        Self {
            job_store,
            task_store,
            runtime,
            config,
            event_sink: None,
            approval_gate: None,
            in_flight: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Attach an event sink that will receive every [`RunEvent`] emitted while
    /// driving a scheduled job.
    pub fn with_event_sink(mut self, sink: CronEventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Attach an approval gate that every scheduled run will use. When the
    /// gate auto-approves, unattended cron jobs can execute tools that would
    /// otherwise prompt for approval and time out.
    pub fn with_approval_gate(mut self, gate: Arc<ApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self
    }

    /// Add a new recurring or one-shot cron job.
    pub async fn add(&self, req: AddJobRequest) -> Result<CronJob, CronError> {
        create_job(&*self.job_store, req).await
    }

    /// List all cron jobs.
    pub async fn list(&self) -> Result<Vec<CronJob>, CronError> {
        self.job_store.list().await
    }

    /// Fetch a single cron job by id.
    pub async fn get_job(&self, id: &str) -> Result<Option<CronJob>, CronError> {
        self.job_store.get(id).await
    }

    /// Remove a cron job by id.
    pub async fn remove(&self, id: &str) -> Result<(), CronError> {
        self.job_store.remove(id).await
    }

    /// Trigger a job manually by id.
    pub async fn run(&self, id: &str) -> Result<Task, CronError> {
        let job = self
            .job_store
            .get(id)
            .await?
            .ok_or_else(|| CronError::NotFound(id.to_string()))?;
        if !self.acquire_job(&job.id).await {
            return Err(CronError::AlreadyRunning(job.id.clone()));
        }
        let result = self.execute(job).await;
        self.release_job(id).await;
        result
    }

    /// Check for jobs that are due and execute them. One-shot jobs are removed
    /// after execution.
    pub async fn tick(&self) -> Vec<Result<Task, CronError>> {
        let now = Utc::now();
        let jobs = match self.job_store.list().await {
            Ok(jobs) => jobs,
            Err(err) => return vec![Err(err)],
        };

        let mut results = Vec::new();
        for mut job in jobs {
            if !job.enabled {
                continue;
            }
            if let Some(next_run) = job.next_run {
                if next_run <= now {
                    if !self.acquire_job(&job.id).await {
                        warn!(job_id = %job.id, "skipping tick; job already running");
                        continue;
                    }
                    let result = self.execute(job.clone()).await;
                    self.release_job(&job.id).await;
                    if job.is_one_shot() {
                        if let Err(err) = self.job_store.remove(&job.id).await {
                            warn!(job_id = %job.id, error = %err, "failed to remove one-shot job");
                        }
                    } else {
                        let job_id = job.id.clone();
                        job.last_run = Some(now);
                        job.refresh_next_run();
                        if let Err(err) = self.job_store.update(job).await {
                            warn!(job_id = %job_id, error = %err, "failed to update cron job");
                        }
                    }
                    results.push(result);
                }
            }
        }
        results
    }

    async fn acquire_job(&self, job_id: &str) -> bool {
        self.in_flight.lock().await.insert(job_id.to_string())
    }

    async fn release_job(&self, job_id: &str) {
        self.in_flight.lock().await.remove(job_id);
    }

    async fn execute(&self, job: CronJob) -> Result<Task, CronError> {
        let task_id = format!(
            "task-cron-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let mut task = Task::new(&task_id, TaskKind::Cron, &job.agent_id);
        task.session_id = Some(session_key_for_cron(&job));
        task.mark_running();
        self.task_store.create(task.clone()).await?;

        let model_ref = resolve_agent_model(&self.config, &job.agent_id);
        let mut request = RunRequest::new(
            task.session_id.clone().unwrap_or_default(),
            &job.agent_id,
            &job.message,
            model_ref,
        )
        .with_system_prompt(format!(
            "You are running a scheduled cron job ({}). Execute the following instruction:",
            job.id
        ))
        // Cron runs are unattended; do not claim an interactive human is present.
        .with_interactive(false);
        if let Some(gate) = &self.approval_gate {
            request = request.with_approval_gate(gate.clone());
        }

        let mut stream = match self.runtime.run(request) {
            Ok(stream) => stream,
            Err(err) => {
                task.mark_failed(err.to_string());
                self.task_store.update(task.clone()).await?;
                return Err(CronError::Runtime(err.to_string()));
            }
        };

        // Drive the stream to completion so the task record captures outcome.
        let mut saw_error = None;
        while let Some(event) = stream.next().await {
            if let Some(sink) = &self.event_sink {
                sink(&task_id, event.clone());
            }
            if let legion_runtime::RunEvent::Lifecycle {
                phase: legion_runtime::LifecyclePhase::Error,
                error,
            } = &event
            {
                saw_error = error.clone();
                break;
            }
        }

        if let Some(error) = saw_error {
            task.mark_failed(error);
        } else {
            task.mark_completed();
        }
        task.run_id = Some(task_id.clone());
        self.task_store.update(task.clone()).await?;
        Ok(task)
    }
}

fn validate_cron(expression: &str) -> Result<(), CronError> {
    let normalized = normalize_cron_expression(expression)?;
    Schedule::from_str(&normalized)
        .map(|_| ())
        .map_err(|e| CronError::InvalidExpression(e.to_string()))
}

/// Accept both standard 5-field cron expressions and cron-crate 6-field
/// expressions (with seconds). For 5-field input, prepend `0` to run at the
/// start of the matching minute.
fn normalize_cron_expression(expression: &str) -> Result<String, CronError> {
    let trimmed = expression.trim();
    let field_count = trimmed.split_whitespace().count();
    match field_count {
        5 => Ok(format!("0 {}", trimmed)),
        6 => Ok(trimmed.to_string()),
        _ => Err(CronError::InvalidExpression(format!(
            "cron expression must have 5 or 6 fields, got {field_count}"
        ))),
    }
}

fn validate_at(at: DateTime<Utc>) -> Result<(), CronError> {
    if at < Utc::now() - ChronoDuration::minutes(1) {
        return Err(CronError::InvalidExpression(
            "one-shot time is in the past".to_string(),
        ));
    }
    Ok(())
}

fn session_key_for_cron(job: &CronJob) -> String {
    legion_plugin_sdk::session_key::direct_session_key(
        &job.agent_id,
        "cron",
        "cron",
        "default",
        &job.id,
    )
}

/// Verify a GitHub-style webhook signature header against the request body.
///
/// The header must have the form `sha256=<hex>` where `<hex>` is the
/// HMAC-SHA256 of `body` keyed by `secret`. The comparison is constant-time to
/// avoid leaking the expected signature through timing.
pub fn verify_webhook_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(provided) = decode_hex(hex_sig) else {
        return false;
    };
    // HMAC accepts keys of any length, so this cannot fail in practice; avoid
    // panicking regardless.
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    use hmac::Mac;
    mac.update(body);
    let computed = mac.finalize().into_bytes();
    constant_time_eq(&computed, &provided)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.as_bytes().chunks(2) {
        let hi = hex_value(pair[0])?;
        let lo = hex_value(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Parse an `--at` datetime string in local time.
pub fn parse_at(input: &str) -> Result<DateTime<Utc>, CronError> {
    // Accept ISO-8601 / RFC3339 or a simple local-time format.
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M:%S") {
        return Ok(Local
            .from_local_datetime(&naive)
            .single()
            .unwrap_or_else(Local::now)
            .with_timezone(&Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S") {
        return Ok(Local
            .from_local_datetime(&naive)
            .single()
            .unwrap_or_else(Local::now)
            .with_timezone(&Utc));
    }
    Err(CronError::InvalidExpression(format!(
        "unable to parse --at datetime: {input}"
    )))
}

/// Background loop that wakes up every minute to evaluate cron schedules.
pub async fn cron_loop(scheduler: Arc<CronScheduler>) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let results = scheduler.tick().await;
        for result in results {
            match result {
                Ok(task) => info!(task_id = %task.id, "cron job executed"),
                Err(err) => warn!(error = %err, "cron execution failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    async fn test_scheduler(fail: bool) -> (CronScheduler, tempfile::TempDir) {
        let (scheduler, _job_store, _task_store, dir) = test_scheduler_with_stores(fail).await;
        (scheduler, dir)
    }

    async fn test_scheduler_with_stores(
        fail: bool,
    ) -> (
        CronScheduler,
        SharedCronJobStore,
        SharedTaskStore,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let job_store: SharedCronJobStore = Arc::new(
            JsonlCronJobStore::open(dir.path().join("cron.jsonl"))
                .await
                .unwrap(),
        );
        let task_store: SharedTaskStore = Arc::new(
            crate::tasks::JsonlTaskStore::open(dir.path().join("tasks.jsonl"))
                .await
                .unwrap(),
        );
        let config = legion_core::config::Config::from_json(
            r#"{ "gateway": { "auth": { "token": "x" } } }"#,
        )
        .unwrap();
        let scheduler = CronScheduler::new(
            job_store.clone(),
            task_store.clone(),
            Arc::new(FakeHarness { fail }),
            config,
        );
        (scheduler, job_store, task_store, dir)
    }

    #[test]
    fn should_parse_standard_cron_expression() {
        let expr = normalize_cron_expression("0 9 * * *").unwrap();
        let schedule = Schedule::from_str(&expr).expect("valid cron");
        let next = schedule.after(&Utc::now()).next();
        assert!(next.is_some());
    }

    #[test]
    fn should_normalize_five_field_cron_expression() {
        assert_eq!(
            normalize_cron_expression("0 9 * * *").unwrap(),
            "0 0 9 * * *"
        );
        assert_eq!(
            normalize_cron_expression("* * * * *").unwrap(),
            "0 * * * * *"
        );
    }

    #[test]
    fn should_reject_malformed_cron_expression() {
        assert!(normalize_cron_expression("too few").is_err());
        assert!(normalize_cron_expression("1 2 3 4 5 6 7 8").is_err());
    }

    #[test]
    fn should_calculate_next_run_for_cron_job() {
        let job = CronJob {
            id: "j1".to_string(),
            agent_id: "main".to_string(),
            message: "daily".to_string(),
            name: String::new(),
            schedule: "0 9 * * *".to_string(),
            at: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            webhook_secret: None,
        };
        let next = job.compute_next_run(Utc::now());
        assert!(next.is_some());
    }

    #[test]
    fn cron_fields_are_interpreted_in_local_time() {
        use chrono::Timelike;
        // `30 12 * * *` (12:30 every day) must fire at 12:30 local, not 12:30 UTC.
        let job = CronJob {
            id: "j-local".to_string(),
            agent_id: "main".to_string(),
            message: "lunch".to_string(),
            name: String::new(),
            schedule: "30 12 * * *".to_string(),
            at: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            webhook_secret: None,
        };
        let next = job
            .compute_next_run(Utc::now())
            .expect("next run computable");
        let local_next = next.with_timezone(&Local);
        assert_eq!(local_next.hour(), 12);
        assert_eq!(local_next.minute(), 30);
    }

    #[test]
    fn should_calculate_next_run_for_one_shot_job() {
        let future = Utc::now() + ChronoDuration::hours(2);
        let job = CronJob {
            id: "j2".to_string(),
            agent_id: "main".to_string(),
            message: "once".to_string(),
            name: String::new(),
            schedule: "__at__".to_string(),
            at: Some(future),
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            webhook_secret: None,
        };
        assert_eq!(job.compute_next_run(Utc::now()), Some(future));
        assert_eq!(job.compute_next_run(future), None);
    }

    #[tokio::test]
    async fn save_writes_atomically_without_temp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.jsonl");
        let store = JsonlCronJobStore::open(&path).await.unwrap();
        let job = CronJob {
            id: "j-atomic".to_string(),
            agent_id: "main".to_string(),
            message: "hello".to_string(),
            name: String::new(),
            schedule: "0 9 * * *".to_string(),
            at: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            webhook_secret: None,
        };
        store.create(job).await.unwrap();

        // The target file holds the full record.
        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let parsed: CronJob = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed.id, "j-atomic");

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

    #[tokio::test]
    async fn separate_handles_at_same_path_share_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.jsonl");

        let tool_store = JsonlCronJobStore::open(&path).await.unwrap();
        let scheduler_store = JsonlCronJobStore::open(&path).await.unwrap();

        let job = CronJob {
            id: "shared-1".to_string(),
            agent_id: "main".to_string(),
            message: "hello".to_string(),
            name: String::new(),
            schedule: "*/1 * * * *".to_string(),
            at: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            webhook_secret: None,
        };
        tool_store.create(job).await.unwrap();

        let jobs = scheduler_store.list().await.unwrap();
        assert!(
            jobs.iter().any(|j| j.id == "shared-1"),
            "second handle must see the first handle's write: {jobs:?}"
        );
    }

    #[tokio::test]
    async fn should_add_recurring_job() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let job = scheduler
            .add(AddJobRequest {
                schedule: "0 9 * * *".to_string(),
                agent_id: "main".to_string(),
                message: "daily report".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(job.agent_id, "main");
        assert_eq!(job.message, "daily report");
        assert!(job.next_run.is_some());
    }

    #[tokio::test]
    async fn add_with_id_prefix_uses_prefix() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let job = scheduler
            .add(AddJobRequest {
                schedule: "0 9 * * *".to_string(),
                agent_id: "main".to_string(),
                message: "daily report".to_string(),
                id_prefix: Some("session-cron".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(
            job.id.starts_with("session-cron-"),
            "expected session-cron prefix, got {}",
            job.id
        );
    }

    #[tokio::test]
    async fn should_add_one_shot_job() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let at = Utc::now() + ChronoDuration::minutes(5);
        let job = scheduler
            .add(AddJobRequest {
                schedule: "__at__".to_string(),
                agent_id: "main".to_string(),
                message: "once".to_string(),
                at: Some(at),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(job.is_one_shot());
        assert_eq!(job.next_run, Some(at));
    }

    #[tokio::test]
    async fn should_reject_invalid_cron() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let result = scheduler
            .add(AddJobRequest {
                schedule: "not a cron".to_string(),
                agent_id: "main".to_string(),
                message: "x".to_string(),
                ..Default::default()
            })
            .await;
        assert!(matches!(result, Err(CronError::InvalidExpression(_))));
    }

    #[tokio::test]
    async fn should_run_job_and_create_task() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let job = scheduler
            .add(AddJobRequest {
                schedule: "0 9 * * *".to_string(),
                agent_id: "main".to_string(),
                message: "ping".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let task = scheduler.run(&job.id).await.unwrap();
        assert_eq!(task.kind, TaskKind::Cron);
        assert_eq!(task.agent_id, "main");
        assert_eq!(task.status, crate::tasks::TaskStatus::Completed);
    }

    #[tokio::test]
    async fn should_mark_task_failed_on_runtime_error() {
        let (scheduler, _dir) = test_scheduler(true).await;
        let job = scheduler
            .add(AddJobRequest {
                schedule: "0 9 * * *".to_string(),
                agent_id: "main".to_string(),
                message: "ping".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let task = scheduler.run(&job.id).await.unwrap();
        assert_eq!(task.status, crate::tasks::TaskStatus::Failed);
        assert!(task.error.unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn should_reject_duplicate_job_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlCronJobStore::open(dir.path().join("cron.jsonl"))
            .await
            .unwrap();
        let job = CronJob {
            id: "cron-1".to_string(),
            agent_id: "main".to_string(),
            message: "first".to_string(),
            name: String::new(),
            schedule: "0 9 * * *".to_string(),
            at: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            webhook_secret: None,
        };
        store.create(job.clone()).await.unwrap();

        let mut duplicate = job;
        duplicate.message = "second".to_string();
        let err = store.create(duplicate).await.unwrap_err();
        assert!(matches!(err, CronError::DuplicateId(id) if id == "cron-1"));
    }

    #[tokio::test]
    async fn should_skip_tick_when_job_is_already_running() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let job = scheduler
            .add(AddJobRequest {
                schedule: "* * * * * *".to_string(),
                agent_id: "main".to_string(),
                message: "ping".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        // Manually mark the job as in-flight.
        assert!(scheduler.acquire_job(&job.id).await);

        // tick() should see the job as due but skip it because it is running.
        let results = scheduler.tick().await;
        assert!(results.is_empty());

        // A second manual run should also be rejected.
        let err = scheduler.run(&job.id).await.unwrap_err();
        assert!(matches!(err, CronError::AlreadyRunning(id) if id == job.id));

        scheduler.release_job(&job.id).await;
        // After release the job can run normally.
        let task = scheduler.run(&job.id).await.unwrap();
        assert_eq!(task.kind, TaskKind::Cron);
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256={hex}")
    }

    #[test]
    fn should_accept_valid_webhook_signature() {
        let body = br#"{"event":"push"}"#;
        let header = sign("s3cret", body);
        assert!(verify_webhook_signature("s3cret", body, &header));
    }

    #[test]
    fn should_reject_webhook_signature_with_wrong_secret() {
        let body = b"payload";
        let header = sign("other-secret", body);
        assert!(!verify_webhook_signature("s3cret", body, &header));
    }

    #[test]
    fn should_reject_webhook_signature_for_tampered_body() {
        let header = sign("s3cret", b"original");
        assert!(!verify_webhook_signature("s3cret", b"tampered", &header));
    }

    #[test]
    fn should_reject_malformed_webhook_signature_header() {
        let body = b"payload";
        assert!(!verify_webhook_signature("s3cret", body, ""));
        assert!(!verify_webhook_signature("s3cret", body, "sha1=abcd"));
        assert!(!verify_webhook_signature("s3cret", body, "sha256=zzzz"));
        assert!(!verify_webhook_signature("s3cret", body, "sha256=abc"));
    }

    #[tokio::test]
    async fn should_add_webhook_only_job_without_cron_validation() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let job = scheduler
            .add(AddJobRequest {
                schedule: "__webhook__".to_string(),
                agent_id: "main".to_string(),
                message: "deploy".to_string(),
                webhook_secret: Some("s3cret".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(job.schedule, "__webhook__");
        assert_eq!(job.webhook_secret, Some("s3cret".to_string()));
        // Webhook-only jobs never fire on the clock.
        assert!(job.next_run.is_none());
        assert!(job.compute_next_run(Utc::now()).is_none());
    }

    #[tokio::test]
    async fn should_reject_webhook_schedule_without_secret() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let result = scheduler
            .add(AddJobRequest {
                schedule: "__webhook__".to_string(),
                agent_id: "main".to_string(),
                message: "deploy".to_string(),
                ..Default::default()
            })
            .await;
        assert!(matches!(result, Err(CronError::InvalidExpression(_))));
    }

    #[tokio::test]
    async fn should_get_job_by_id() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let job = scheduler
            .add(AddJobRequest {
                schedule: "0 9 * * *".to_string(),
                agent_id: "main".to_string(),
                message: "ping".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let fetched = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, job.id);
        assert!(scheduler.get_job("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tick_executes_due_one_shot_and_removes_it() {
        let (scheduler, job_store, task_store, _dir) = test_scheduler_with_stores(false).await;
        let due = Utc::now() - ChronoDuration::minutes(1);
        let job = CronJob {
            id: "one-shot-1".to_string(),
            agent_id: "main".to_string(),
            message: "once".to_string(),
            name: String::new(),
            schedule: "__at__".to_string(),
            at: Some(due),
            enabled: true,
            created_at: Utc::now(),
            next_run: Some(due),
            last_run: None,
            webhook_secret: None,
        };
        job_store.create(job).await.unwrap();

        let results = scheduler.tick().await;

        assert_eq!(results.len(), 1);
        let task = results[0].as_ref().expect("one-shot execution succeeds");
        assert_eq!(task.kind, TaskKind::Cron);
        assert_eq!(task.status, crate::tasks::TaskStatus::Completed);
        let tasks = task_store.list().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task.id);
        // One-shot jobs are removed from the store after firing.
        assert!(scheduler.get_job("one-shot-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tick_refreshes_recurring_job_next_run() {
        let (scheduler, job_store, task_store, _dir) = test_scheduler_with_stores(false).await;
        let due = Utc::now() - ChronoDuration::minutes(1);
        let job = CronJob {
            id: "recurring-1".to_string(),
            agent_id: "main".to_string(),
            message: "daily".to_string(),
            name: String::new(),
            schedule: "0 9 * * *".to_string(),
            at: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: Some(due),
            last_run: None,
            webhook_secret: None,
        };
        job_store.create(job).await.unwrap();

        let results = scheduler.tick().await;

        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
        assert_eq!(task_store.list().await.unwrap().len(), 1);
        // Recurring jobs survive the tick with a refreshed schedule.
        let after = scheduler
            .get_job("recurring-1")
            .await
            .unwrap()
            .expect("recurring job still exists");
        let last_run = after.last_run.expect("last run recorded");
        assert!(last_run > due);
        let next_run = after.next_run.expect("next run recomputed");
        assert!(next_run > Utc::now(), "next run must be in the future");
    }

    #[tokio::test]
    async fn tick_skips_disabled_jobs() {
        let (scheduler, job_store, task_store, _dir) = test_scheduler_with_stores(false).await;
        let due = Utc::now() - ChronoDuration::minutes(1);
        let job = CronJob {
            id: "disabled-1".to_string(),
            agent_id: "main".to_string(),
            message: "never".to_string(),
            name: String::new(),
            schedule: "0 9 * * *".to_string(),
            at: None,
            enabled: false,
            created_at: Utc::now(),
            next_run: Some(due),
            last_run: None,
            webhook_secret: None,
        };
        job_store.create(job).await.unwrap();

        let results = scheduler.tick().await;

        assert!(results.is_empty(), "disabled jobs must not execute");
        assert!(task_store.list().await.unwrap().is_empty());
        let after = scheduler
            .get_job("disabled-1")
            .await
            .unwrap()
            .expect("disabled job untouched");
        assert_eq!(after.next_run, Some(due));
        assert_eq!(after.last_run, None);
    }

    #[tokio::test]
    async fn add_rejects_at_in_the_past() {
        let (scheduler, _dir) = test_scheduler(false).await;
        let result = scheduler
            .add(AddJobRequest {
                schedule: "__at__".to_string(),
                agent_id: "main".to_string(),
                message: "too late".to_string(),
                at: Some(Utc::now() - ChronoDuration::hours(1)),
                ..Default::default()
            })
            .await;
        assert!(matches!(result, Err(CronError::InvalidExpression(_))));
    }

    #[test]
    fn parse_at_accepts_rfc3339_and_local_formats() {
        // RFC3339 / ISO-8601 with an explicit offset.
        let rfc3339 = parse_at("2026-07-13T09:00:00Z").unwrap();
        assert_eq!(
            rfc3339,
            DateTime::parse_from_rfc3339("2026-07-13T09:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        let offset = parse_at("2026-07-13T09:00:00+02:00").unwrap();
        assert_eq!(
            offset,
            DateTime::parse_from_rfc3339("2026-07-13T07:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );

        // Naive local-time formats are interpreted in the local timezone.
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 7, 13)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        for input in ["2026-07-13 09:00:00", "2026-07-13T09:00:00"] {
            let parsed = parse_at(input).unwrap();
            assert_eq!(parsed.with_timezone(&Local).naive_local(), naive);
        }
    }

    #[test]
    fn parse_at_rejects_garbage() {
        for input in ["tomorrow at noon", "", "2026-07-13", "2026-13-99 99:99:99"] {
            assert!(
                matches!(parse_at(input), Err(CronError::InvalidExpression(_))),
                "expected {input:?} to be rejected"
            );
        }
    }
}
