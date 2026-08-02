use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use thiserror::Error;
use tokio::sync::{Semaphore, oneshot};
use tracing::{Instrument, info_span};

use crate::agent_loop::AgentRuntime;
use crate::context::sessions_dir;
use crate::types::{LifecyclePhase, RunEvent, RunRequest, RunStream};
use legion_core::config::SubagentConfig;
use legion_provider::model_ref::resolve_agent_model;

/// Kind of sub-agent to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentKind {
    /// An isolated-context agent addressed by agent type (an entry in
    /// `agents.list`, falling back to `main`). The child receives only the
    /// supplied prompt and optional system prompt, never the parent's history.
    Typed(String),
    /// A fork of the parent agent: same agent id, inheriting the parent's
    /// conversation history (a snapshot taken when `spawn_subagent` executes),
    /// workspace, and provider router. The child continues from that context
    /// with only the supplied prompt appended.
    Fork,
}

/// A request to delegate work to a child agent.
#[derive(Debug, Clone)]
pub struct SubagentRequest {
    pub kind: SubagentKind,
    pub prompt: String,
    /// Optional model override for the child run.
    pub model: Option<String>,
    /// Tool names the child is allowed to use. Must be a subset of the parent's
    /// tools (enforced by `spawn_subagent`). `None` means "inherit / no further
    /// narrowing" (the child gets the parent's effective set); `Some(vec![])`
    /// means "no tools at all".
    pub allowed_tools: Option<Vec<String>>,
    /// Agent id of the parent run (used as the child id for [`SubagentKind::Fork`]).
    pub parent_agent_id: String,
    /// Nesting depth of the parent run; the child runs at `depth + 1`.
    pub parent_depth: u8,
    /// Optional system prompt injected into the child run.
    pub system_prompt: Option<String>,
    /// Prior turns carried for continuity; honored for both kinds. `Fork`
    /// children receive the parent's snapshot, `Typed` children may carry an
    /// accumulated per-teammate history (swarm Phase D). Empty means a fresh
    /// conversation.
    pub history: Vec<legion_provider::types::ChatMessage>,
    /// Per-child iteration cap override; defaults to `subagents.default_max_iterations`.
    /// `None` (the config default) means no cap: the child falls back to the
    /// runtime's own `max_iterations`, bounded by the wall-clock timeout.
    pub max_iterations: Option<usize>,
    /// Per-child timeout override; defaults to `subagents.default_timeout_ms`.
    pub timeout: Option<Duration>,
}

/// Outcome of a sub-agent run, returned to the parent as a tool result.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub handle_id: String,
    pub text: String,
    pub tool_call_count: usize,
    pub transcript_path: Option<PathBuf>,
    pub status: SubagentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    Completed,
    Failed(String),
    TimedOut,
    Aborted,
}

impl std::fmt::Display for SubagentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubagentStatus::Completed => write!(f, "completed"),
            SubagentStatus::Failed(e) => write!(f, "failed: {e}"),
            SubagentStatus::TimedOut => write!(f, "timed_out"),
            SubagentStatus::Aborted => write!(f, "aborted"),
        }
    }
}

#[derive(Debug, Error)]
pub enum SubagentError {
    #[error("sub-agent depth limit reached (depth {depth} >= max {max})")]
    DepthLimit { depth: u8, max: u8 },
    #[error("sub-agent concurrency limit reached")]
    Concurrency,
    #[error("invalid sub-agent request: {0}")]
    Validation(String),
    #[error("sub-agent join failed: {0}")]
    Join(String),
}

/// Handle returned by [`SubagentSpawner::spawn`]; `join` awaits the child result.
pub struct SubagentHandle {
    pub id: String,
    rx: oneshot::Receiver<SubagentResult>,
}

impl SubagentHandle {
    /// Construct a handle from a oneshot receiver; used by `SubagentSpawner`
    /// implementations (including out-of-crate ones) to hand back a result.
    pub fn from_receiver(id: String, rx: oneshot::Receiver<SubagentResult>) -> Self {
        Self { id, rx }
    }

    pub async fn join(self) -> Result<SubagentResult, SubagentError> {
        self.rx
            .await
            .map_err(|e| SubagentError::Join(e.to_string()))
    }
}

/// Trait for delegating work to a child agent (multi-agent Phase A).
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    async fn spawn(&self, req: SubagentRequest) -> Result<SubagentHandle, SubagentError>;
}

/// [`SubagentSpawner`] backed by the in-process [`AgentRuntime`]. Each spawn
/// reuses `AgentRuntime::run` on a fresh, isolated `RunRequest`, bounded by a
/// concurrency semaphore, an iteration cap, and a wall-clock timeout.
pub struct RuntimeSubagentSpawner {
    runtime: Arc<AgentRuntime>,
    cfg: SubagentConfig,
    sem: Arc<Semaphore>,
    /// Override for the sidechain transcript directory. `None` (the
    /// production default) resolves `~/.legion/agents/<child>/sessions`;
    /// tests inject a tempdir so they never touch the real home directory.
    sessions_dir: Option<PathBuf>,
}

impl RuntimeSubagentSpawner {
    pub fn new(runtime: Arc<AgentRuntime>, cfg: SubagentConfig) -> Self {
        let permits = cfg.max_concurrent.max(1);
        Self {
            runtime,
            cfg,
            sem: Arc::new(Semaphore::new(permits)),
            sessions_dir: None,
        }
    }

    /// Override the directory sidechain transcripts are written to (tests).
    pub fn with_sessions_dir(mut self, dir: PathBuf) -> Self {
        self.sessions_dir = Some(dir);
        self
    }
}

#[async_trait]
impl SubagentSpawner for RuntimeSubagentSpawner {
    async fn spawn(&self, req: SubagentRequest) -> Result<SubagentHandle, SubagentError> {
        if req.parent_depth.saturating_add(1) > self.cfg.max_depth {
            return Err(SubagentError::DepthLimit {
                depth: req.parent_depth.saturating_add(1),
                max: self.cfg.max_depth,
            });
        }

        // Bound the permit wait by the child's effective timeout: without it a
        // nested fan-out (a child spawning its own sub-agent while every
        // permit is held by siblings doing the same) deadlocks the parent's
        // turn, because the wall-clock timeout only starts after acquisition.
        let acquire_timeout = req
            .timeout
            .unwrap_or_else(|| Duration::from_millis(self.cfg.default_timeout_ms));
        let permit =
            match tokio::time::timeout(acquire_timeout, self.sem.clone().acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) | Err(_) => return Err(SubagentError::Concurrency),
            };

        let handle_id = next_handle_id();
        let (tx, rx) = oneshot::channel();
        let runtime = self.runtime.clone();
        let cfg = self.cfg.clone();
        let sessions_dir = self.sessions_dir.clone();
        let handle_for_task = handle_id.clone();

        tokio::spawn(async move {
            let _permit = permit; // hold the semaphore until the child finishes
            let result = run_child(runtime, cfg, req, handle_for_task, sessions_dir).await;
            let _ = tx.send(result);
        });

        Ok(SubagentHandle::from_receiver(handle_id, rx))
    }
}

async fn run_child(
    runtime: Arc<AgentRuntime>,
    cfg: SubagentConfig,
    req: SubagentRequest,
    handle_id: String,
    sessions_dir: Option<PathBuf>,
) -> SubagentResult {
    let child = match &req.kind {
        SubagentKind::Typed(name) => name.clone(),
        SubagentKind::Fork => req.parent_agent_id.clone(),
    };
    let child_depth = req.parent_depth.saturating_add(1);
    let session_id = legion_plugin_sdk::session_key::direct_session_key(
        &child, "subagent", "spawn", "local", &handle_id,
    );

    // Validate an explicit model override up front so a bogus value fails
    // fast with recovery guidance instead of burning a child run on the same
    // provider error. The config-derived default keeps its existing
    // resolve-at-chat behavior.
    if let Some(explicit) = &req.model {
        let invalid = match runtime.router_for(&child) {
            Some(router) => router
                .validate_model_ref(explicit)
                .err()
                .map(|e| e.to_string()),
            None => Some(format!("no provider router for agent '{child}'")),
        };
        if let Some(err) = invalid {
            return SubagentResult {
                handle_id,
                text: String::new(),
                tool_call_count: 0,
                transcript_path: None,
                status: SubagentStatus::Failed(format!(
                    "invalid model '{explicit}': {err}. Omit the model parameter to inherit \
                     the default model, or pass a configured alias / provider-model ref"
                )),
            };
        }
    }

    let model_ref = req
        .model
        .clone()
        .unwrap_or_else(|| resolve_agent_model(runtime.config(), &child));
    // `None` (the config default) leaves the run uncapped: the child then
    // falls back to the runtime's own `max_iterations`, same as the parent.
    let eff_iter = req.max_iterations.or(cfg.default_max_iterations);
    let timeout_dur = req
        .timeout
        .unwrap_or_else(|| Duration::from_millis(cfg.default_timeout_ms));

    let mut request = RunRequest::new(&session_id, &child, &req.prompt, model_ref)
        .with_interactive(false)
        .with_depth(child_depth);
    if let Some(cap) = eff_iter {
        request = request.with_max_iterations(cap);
    }
    if !req.history.is_empty() {
        request = request.with_history(req.history.clone());
    }
    if let Some(allowed) = req.allowed_tools.clone() {
        request = request.with_allowed_tools(allowed);
    }
    if let Some(prompt) = req.system_prompt.clone() {
        request = request.with_system_prompt(prompt);
    }

    let span = info_span!("subagent", handle = %handle_id, agent = %child, depth = child_depth);
    let outcome = drive(&runtime, request, timeout_dur).instrument(span).await;

    let (text, tool_call_count, status, events) = match outcome {
        // A timed-out child keeps the events it collected before the
        // deadline: the sidechain transcript is the only window into where
        // it got stuck.
        Ok((_, _, ev, _, true)) => (String::new(), 0, SubagentStatus::TimedOut, ev),
        Ok((t, c, ev, None, false)) => (t, c, SubagentStatus::Completed, ev),
        Ok((t, c, ev, Some(err), false)) => (t, c, SubagentStatus::Failed(err), ev),
        Err(err) => (String::new(), 0, SubagentStatus::Failed(err), Vec::new()),
    };

    let transcript_path =
        write_sidechain(&child, &handle_id, &events, sessions_dir.as_deref()).await;

    SubagentResult {
        handle_id,
        text,
        tool_call_count,
        transcript_path,
        status,
    }
}

async fn drive(
    runtime: &AgentRuntime,
    request: RunRequest,
    timeout_dur: Duration,
) -> Result<(String, usize, Vec<RunEvent>, Option<String>, bool), String> {
    let stream = runtime.run(request).map_err(|e| e.to_string())?;
    Ok(collect(stream, timeout_dur).await)
}

/// Drain the run's event stream, accumulating the child's text and events.
/// The wall-clock deadline is enforced per iteration (rather than wrapping
/// the whole future) so a timeout keeps the events collected so far; the
/// final bool reports whether the deadline fired.
async fn collect(
    stream: RunStream,
    timeout_dur: Duration,
) -> (String, usize, Vec<RunEvent>, Option<String>, bool) {
    tokio::pin!(stream);
    let deadline = tokio::time::Instant::now() + timeout_dur;
    let mut text = String::new();
    let mut tool_call_count = 0usize;
    let mut events: Vec<RunEvent> = Vec::new();
    let mut err: Option<String> = None;

    loop {
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(next) => next,
            Err(_elapsed) => return (String::new(), 0, events, None, true),
        };
        let Some(ev) = next else { break };
        match &ev {
            RunEvent::AssistantDelta { delta } => text.push_str(delta),
            RunEvent::ToolEnd { .. } => tool_call_count += 1,
            RunEvent::Lifecycle {
                phase: LifecyclePhase::Error,
                error,
            } => err = error.clone(),
            _ => {}
        }
        let stop = matches!(
            ev,
            RunEvent::Lifecycle {
                phase: LifecyclePhase::End | LifecyclePhase::Error,
                ..
            }
        );
        events.push(ev);
        if stop {
            break;
        }
    }

    (text, tool_call_count, events, err, false)
}

async fn write_sidechain(
    child: &str,
    handle_id: &str,
    events: &[RunEvent],
    sessions_base: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let dir = sessions_base
        .map(PathBuf::from)
        .unwrap_or_else(|| sessions_dir(child));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(error = %e, "subagent sidechain: failed to create sessions dir");
        return None;
    }
    let path = dir.join(format!("subagent-{handle_id}.jsonl"));
    let mut buf = String::new();
    for ev in events {
        match serde_json::to_string(ev) {
            Ok(line) => {
                buf.push_str(&line);
                buf.push('\n');
            }
            Err(e) => tracing::warn!(error = %e, "subagent sidechain: failed to serialize event"),
        }
    }
    match tokio::fs::write(&path, buf).await {
        Ok(_) => Some(path),
        Err(e) => {
            tracing::warn!(error = %e, "subagent sidechain: failed to write transcript");
            None
        }
    }
}

fn next_handle_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("sub-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryBackend, MemoryError, MemoryMeta, MemoryNote};
    use crate::tools::{Tool, ToolRegistry};
    use async_trait::async_trait;
    use legion_core::config::Config;
    use legion_provider::provider::Provider;
    use legion_provider::router::ProviderRouter;
    use legion_provider::types::{
        ChatChunk, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding, FinishReason,
        ModelInfo, ProviderError, ToolDefinition,
    };

    struct EmptyRegistry;

    #[async_trait]
    impl ToolRegistry for EmptyRegistry {
        fn get(&self, _name: &str) -> Option<Arc<dyn Tool>> {
            None
        }
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
    }

    struct NoopMemory;

    #[async_trait]
    impl MemoryBackend for NoopMemory {
        async fn search(
            &self,
            _query: &str,
            _top_k: usize,
        ) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(Vec::new())
        }
        async fn get(
            &self,
            _path: &str,
            _range: Option<std::ops::Range<usize>>,
        ) -> Result<String, MemoryError> {
            Ok(String::new())
        }
        async fn index(
            &self,
            _id: &str,
            _content: &str,
            _meta: MemoryMeta,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    /// Replies "child-answer" when the last user message contains "child",
    /// otherwise "parent-answer".
    struct RoutingProvider;

    #[async_trait]
    impl Provider for RoutingProvider {
        fn id(&self) -> &str {
            "routing"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let last_user = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == ChatRole::User)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let delta = if last_user.contains("child") {
                "child-answer"
            } else {
                "parent-answer"
            };
            let chunk = ChatChunk {
                index: 0,
                delta: delta.to_string(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Never yields a chunk; used to exercise the spawn timeout path.
    struct PendingProvider;

    #[async_trait]
    impl Provider for PendingProvider {
        fn id(&self) -> &str {
            "pending"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            Ok(Box::pin(futures::stream::pending::<
                Result<ChatChunk, ProviderError>,
            >()))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn build_runtime(provider: Arc<dyn Provider>) -> Arc<AgentRuntime> {
        let mut router = ProviderRouter::new();
        router.register_provider(provider);
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#)
            .expect("test config parses");
        Arc::new(AgentRuntime::new(
            Arc::new(router),
            Arc::new(EmptyRegistry),
            Arc::new(NoopMemory),
            config,
        ))
    }

    /// Spawner wired to a tempdir sessions dir so sidechain transcripts never
    /// touch the real `~/.legion` tree. The TempDir must outlive the test.
    fn spawner_with_temp_sessions(
        runtime: Arc<AgentRuntime>,
    ) -> (RuntimeSubagentSpawner, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let spawner = RuntimeSubagentSpawner::new(runtime, SubagentConfig::default())
            .with_sessions_dir(dir.path().to_path_buf());
        (spawner, dir)
    }

    /// Assert the sidechain transcript was written inside the injected dir.
    fn assert_sidechain_in(result: &SubagentResult, dir: &std::path::Path) {
        let path = result
            .transcript_path
            .as_ref()
            .expect("sidechain transcript written");
        assert!(
            path.starts_with(dir),
            "sidechain must land in the injected tempdir, got {path:?}"
        );
    }

    #[test]
    fn handle_ids_are_unique() {
        let a = next_handle_id();
        let b = next_handle_id();
        assert_ne!(a, b);
        assert!(a.starts_with("sub-"));
    }

    /// The session key a child run actually receives must be the 7-segment
    /// `agent:<child>:subagent:spawn:local:direct:<handle>` format. A capture
    /// tool records the `ToolContext.session_id` the runtime passes down, so
    /// this exercises the real `run_child` construction end to end.
    #[tokio::test]
    async fn child_session_key_is_seven_segments() {
        let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let runtime = {
            let mut router = ProviderRouter::new();
            router.register_provider(Arc::new(CaptureSessionProvider));
            let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#)
                .expect("test config parses");
            Arc::new(AgentRuntime::new(
                Arc::new(router),
                Arc::new(VecRegistry {
                    tools: vec![Arc::new(CaptureSessionTool {
                        captured: captured.clone(),
                    })],
                }),
                Arc::new(NoopMemory),
                config,
            ))
        };
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("researcher".into()),
                prompt: "capture the session".into(),
                model: Some("cap-session/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: None,
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::Completed);

        let session_id = captured
            .lock()
            .unwrap()
            .clone()
            .expect("capture tool executed");
        let expected = format!(
            "agent:researcher:subagent:spawn:local:direct:{}",
            result.handle_id
        );
        assert_eq!(session_id, expected);
        assert_eq!(session_id.split(':').count(), 7);
        assert_sidechain_in(&result, dir.path());
    }

    #[test]
    fn status_display_is_stable() {
        assert_eq!(SubagentStatus::Completed.to_string(), "completed");
        assert_eq!(SubagentStatus::TimedOut.to_string(), "timed_out");
        assert_eq!(
            SubagentStatus::Failed("boom".into()).to_string(),
            "failed: boom"
        );
    }

    #[tokio::test]
    async fn spawn_typed_completes_with_child_text() {
        let runtime = build_runtime(Arc::new(RoutingProvider));
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "please do the child task".into(),
                model: Some("routing/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: None,
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::Completed);
        assert!(
            result.text.contains("child-answer"),
            "child text should be collected, got: {:?}",
            result.text
        );
        assert_sidechain_in(&result, dir.path());
    }

    #[tokio::test]
    async fn spawn_depth_limit_rejected() {
        let runtime = build_runtime(Arc::new(RoutingProvider));
        let cfg = SubagentConfig {
            max_depth: 2,
            ..SubagentConfig::default()
        };
        let spawner = RuntimeSubagentSpawner::new(runtime, cfg);
        let err = match spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "too deep".into(),
                model: None,
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 2,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: None,
            })
            .await
        {
            Ok(_) => panic!("depth 3 should exceed max_depth 2"),
            Err(e) => e,
        };
        assert!(matches!(
            err,
            SubagentError::DepthLimit { depth: 3, max: 2 }
        ));
    }

    #[tokio::test]
    async fn spawn_timeout_yields_timed_out() {
        let runtime = build_runtime(Arc::new(PendingProvider));
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "hang".into(),
                model: Some("pending/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: Some(Duration::from_millis(200)),
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::TimedOut);
        // A timed-out child still gets a (possibly empty) sidechain in the
        // injected dir, never in the real home directory.
        assert_sidechain_in(&result, dir.path());
    }

    /// With the permit pool exhausted, a queued spawn must fail explicitly
    /// once its acquire wait exceeds the child timeout instead of hanging the
    /// parent's turn (nested fan-outs would otherwise deadlock the semaphore).
    #[tokio::test]
    async fn spawn_permit_wait_times_out_as_concurrency_error() {
        let runtime = build_runtime(Arc::new(PendingProvider));
        let cfg = SubagentConfig {
            max_concurrent: 1,
            ..SubagentConfig::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let spawner =
            RuntimeSubagentSpawner::new(runtime, cfg).with_sessions_dir(dir.path().to_path_buf());

        // The first child holds the only permit until its own timeout fires
        // (PendingProvider never yields).
        let first = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "hold the permit".into(),
                model: Some("pending/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: Some(Duration::from_millis(1000)),
            })
            .await
            .expect("first spawn accepted");

        let err = match spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "needs a permit".into(),
                model: Some("pending/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: Some(Duration::from_millis(100)),
            })
            .await
        {
            Ok(_) => panic!("second spawn must fail on the exhausted semaphore"),
            Err(e) => e,
        };
        assert!(
            matches!(err, SubagentError::Concurrency),
            "expected Concurrency error, got {err:?}"
        );

        // Drain the first child so its sidechain lands while the tempdir lives.
        let result = first.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::TimedOut);
    }

    /// An explicit model override that does not resolve to a registered
    /// provider fails fast with recovery guidance, without starting a child
    /// run (no transcript is written).
    #[tokio::test]
    async fn spawn_invalid_model_override_fails_fast() {
        let runtime = build_runtime(Arc::new(RoutingProvider));
        let (spawner, _dir) = spawner_with_temp_sessions(runtime);
        let request = |model: &str| SubagentRequest {
            kind: SubagentKind::Typed("main".into()),
            prompt: "junk model".into(),
            model: Some(model.to_string()),
            allowed_tools: None,
            parent_agent_id: "main".into(),
            parent_depth: 0,
            history: Vec::new(),
            system_prompt: None,
            max_iterations: None,
            timeout: None,
        };

        for bad in ["default", "ghost/gpt-4o"] {
            let handle = spawner.spawn(request(bad)).await.expect("spawn accepted");
            let result = handle.join().await.expect("join ok");
            match &result.status {
                SubagentStatus::Failed(err) => {
                    assert!(
                        err.contains(&format!("invalid model '{bad}'")),
                        "error must name the bad ref, got {err:?}"
                    );
                    assert!(
                        err.contains("Omit the model parameter"),
                        "error must guide recovery, got {err:?}"
                    );
                }
                other => panic!("expected Failed status for '{bad}', got {other:?}"),
            }
            assert!(
                result.transcript_path.is_none(),
                "no child run means no transcript"
            );
        }
    }

    /// Emits one chunk and then never finishes; used to verify a timed-out
    /// child keeps the events it collected before the deadline.
    struct ChunkThenPendingProvider;

    #[async_trait]
    impl Provider for ChunkThenPendingProvider {
        fn id(&self) -> &str {
            "chunk-pending"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let chunk = ChatChunk {
                index: 0,
                delta: "partial-answer".to_string(),
                finish_reason: None,
                tool_calls: None,
            };
            let stream = futures::stream::iter(vec![Ok(chunk)])
                .chain(futures::stream::pending::<Result<ChatChunk, ProviderError>>());
            Ok(Box::pin(stream))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn timed_out_child_keeps_partial_event_stream() {
        let runtime = build_runtime(Arc::new(ChunkThenPendingProvider));
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "hang after one chunk".into(),
                model: Some("chunk-pending/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: Some(Duration::from_millis(200)),
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::TimedOut);

        let path = result.transcript_path.expect("transcript written");
        assert!(path.starts_with(dir.path()));
        let contents = std::fs::read_to_string(path).expect("transcript readable");
        assert!(
            contents.contains("partial-answer"),
            "timed-out transcript must keep pre-deadline events, got {contents:?}"
        );
    }

    /// Captures the messages of every chat call; always replies "child-answer".
    struct CapturingChildProvider {
        captured: Arc<std::sync::Mutex<Vec<Vec<legion_provider::types::ChatMessage>>>>,
    }

    #[async_trait]
    impl Provider for CapturingChildProvider {
        fn id(&self) -> &str {
            "cap"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            self.captured.lock().unwrap().push(req.messages.clone());
            let chunk = ChatChunk {
                index: 0,
                delta: "child-answer".to_string(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// First call requests the `locked` tool; afterwards replies "denied-ok"
    /// only if the tool result shows an approval denial.
    struct DenyCheckProvider;

    #[async_trait]
    impl Provider for DenyCheckProvider {
        fn id(&self) -> &str {
            "deny"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let tool_reply = req.messages.iter().find(|m| m.role == ChatRole::Tool);
            let chunk = match tool_reply {
                None => ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    tool_calls: Some(vec![legion_provider::types::ToolCall {
                        id: "call-1".into(),
                        kind: "function".into(),
                        function: legion_provider::types::FunctionCall {
                            name: "locked".into(),
                            arguments: "{}".into(),
                        },
                    }]),
                },
                Some(m) => ChatChunk {
                    index: 0,
                    delta: if m.content.contains("approval denied") {
                        "denied-ok".to_string()
                    } else {
                        format!("no-denial: {}", m.content)
                    },
                    finish_reason: Some(FinishReason::Stop),
                    tool_calls: None,
                },
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Tool that always requires interactive approval.
    struct LockedTool;

    fn required_policy() -> &'static crate::tools::Policy {
        use std::sync::OnceLock;
        static POLICY: OnceLock<crate::tools::Policy> = OnceLock::new();
        POLICY.get_or_init(|| crate::tools::Policy {
            approval: crate::tools::Approval::Required,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }

    #[async_trait]
    impl Tool for LockedTool {
        fn name(&self) -> &str {
            "locked"
        }
        fn description(&self) -> &str {
            "A tool that requires approval."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        fn policy(&self) -> &crate::tools::Policy {
            required_policy()
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: crate::tools::ToolContext,
        ) -> Result<crate::tools::ToolResult, crate::tools::ToolError> {
            Ok(crate::tools::ToolResult::ok("unlocked"))
        }
    }

    struct VecRegistry {
        tools: Vec<Arc<dyn Tool>>,
    }

    #[async_trait]
    impl ToolRegistry for VecRegistry {
        fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
            self.tools.iter().find(|t| t.name() == name).cloned()
        }
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.tools
                .iter()
                .map(|t| crate::tools::ToolDefinitionExt::definition(t.as_ref()))
                .collect()
        }
    }

    #[tokio::test]
    async fn fork_child_inherits_parent_history() {
        let captured: Arc<std::sync::Mutex<Vec<Vec<legion_provider::types::ChatMessage>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let runtime = build_runtime(Arc::new(CapturingChildProvider {
            captured: captured.clone(),
        }));
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Fork,
                prompt: "continue the work".into(),
                model: Some("cap/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                system_prompt: None,
                history: vec![
                    legion_provider::types::ChatMessage::user("from-parent-context"),
                    legion_provider::types::ChatMessage::assistant("parent-answer"),
                ],
                max_iterations: None,
                timeout: None,
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::Completed);

        let contents: Vec<String> = {
            let calls = captured.lock().unwrap();
            calls
                .iter()
                .flat_map(|msgs| msgs.iter().map(|m| m.content.clone()))
                .collect()
        };
        assert!(
            contents.iter().any(|c| c.contains("from-parent-context")),
            "fork child must see the inherited history, got {contents:?}"
        );
        assert_sidechain_in(&result, dir.path());
    }

    #[tokio::test]
    async fn typed_child_honors_supplied_history() {
        // Swarm teammates (Phase D) pass an accumulated per-teammate history
        // to Typed children; run_child must attach it for both kinds.
        let captured: Arc<std::sync::Mutex<Vec<Vec<legion_provider::types::ChatMessage>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let runtime = build_runtime(Arc::new(CapturingChildProvider {
            captured: captured.clone(),
        }));
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "next child turn".into(),
                model: Some("cap/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                system_prompt: None,
                history: vec![
                    legion_provider::types::ChatMessage::user("prior-turn-prompt"),
                    legion_provider::types::ChatMessage::assistant("prior-turn-reply"),
                ],
                max_iterations: None,
                timeout: None,
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::Completed);

        let contents: Vec<String> = {
            let calls = captured.lock().unwrap();
            calls
                .iter()
                .flat_map(|msgs| msgs.iter().map(|m| m.content.clone()))
                .collect()
        };
        assert!(
            contents.iter().any(|c| c.contains("prior-turn-prompt")),
            "typed child must see the supplied history, got {contents:?}"
        );
        assert_sidechain_in(&result, dir.path());
    }

    #[tokio::test]
    async fn child_required_approval_tool_is_denied_unattended() {
        let runtime = {
            let mut router = ProviderRouter::new();
            router.register_provider(Arc::new(DenyCheckProvider));
            let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#)
                .expect("test config parses");
            Arc::new(AgentRuntime::new(
                Arc::new(router),
                Arc::new(VecRegistry {
                    tools: vec![Arc::new(LockedTool)],
                }),
                Arc::new(NoopMemory),
                config,
            ))
        };
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "call the locked tool".into(),
                model: Some("deny/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                system_prompt: None,
                history: Vec::new(),
                max_iterations: None,
                timeout: None,
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        assert_eq!(result.status, SubagentStatus::Completed);
        assert_eq!(result.tool_call_count, 1, "denied call still counts once");
        assert!(
            result.text.contains("denied-ok"),
            "child run must not wait for approval; got {:?}",
            result.text
        );
        assert_sidechain_in(&result, dir.path());
    }

    /// Always fails the chat call; used to exercise the run-error mapping.
    struct FailingProvider;

    #[async_trait]
    impl Provider for FailingProvider {
        fn id(&self) -> &str {
            "fail"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            Err(ProviderError::StreamAborted("provider exploded".into()))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn child_run_error_maps_to_failed_status() {
        let runtime = build_runtime(Arc::new(FailingProvider));
        let (spawner, dir) = spawner_with_temp_sessions(runtime);
        let handle = spawner
            .spawn(SubagentRequest {
                kind: SubagentKind::Typed("main".into()),
                prompt: "boom".into(),
                model: Some("fail/gpt-4o".into()),
                allowed_tools: None,
                parent_agent_id: "main".into(),
                parent_depth: 0,
                history: Vec::new(),
                system_prompt: None,
                max_iterations: None,
                timeout: None,
            })
            .await
            .expect("spawn accepted");
        let result = handle.join().await.expect("join ok");
        match &result.status {
            SubagentStatus::Failed(err) => assert!(
                err.contains("provider exploded"),
                "failure status must carry the provider error, got {err:?}"
            ),
            other => panic!("expected Failed status, got {other:?}"),
        }
        assert_sidechain_in(&result, dir.path());
    }

    /// First call requests the `capture` tool; afterwards replies "done".
    struct CaptureSessionProvider;

    #[async_trait]
    impl Provider for CaptureSessionProvider {
        fn id(&self) -> &str {
            "cap-session"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let tool_reply = req.messages.iter().find(|m| m.role == ChatRole::Tool);
            let chunk = match tool_reply {
                None => ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    tool_calls: Some(vec![legion_provider::types::ToolCall {
                        id: "call-1".into(),
                        kind: "function".into(),
                        function: legion_provider::types::FunctionCall {
                            name: "capture".into(),
                            arguments: "{}".into(),
                        },
                    }]),
                },
                Some(_) => ChatChunk {
                    index: 0,
                    delta: "done".to_string(),
                    finish_reason: Some(FinishReason::Stop),
                    tool_calls: None,
                },
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Records the session id the runtime hands to tool execution. The policy
    /// is `Approval::Off` so the call executes in a non-interactive child run.
    struct CaptureSessionTool {
        captured: Arc<std::sync::Mutex<Option<String>>>,
    }

    fn off_policy() -> &'static crate::tools::Policy {
        use std::sync::OnceLock;
        static POLICY: OnceLock<crate::tools::Policy> = OnceLock::new();
        POLICY.get_or_init(|| crate::tools::Policy {
            approval: crate::tools::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }

    #[async_trait]
    impl Tool for CaptureSessionTool {
        fn name(&self) -> &str {
            "capture"
        }
        fn description(&self) -> &str {
            "Records the session id."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        fn policy(&self) -> &crate::tools::Policy {
            off_policy()
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            ctx: crate::tools::ToolContext,
        ) -> Result<crate::tools::ToolResult, crate::tools::ToolError> {
            *self.captured.lock().unwrap() = Some(ctx.session_id.clone());
            Ok(crate::tools::ToolResult::ok("captured"))
        }
    }
}
