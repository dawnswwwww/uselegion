use std::sync::Arc;

use async_trait::async_trait;

use crate::auto_extract::AutoExtractor;
use crate::commitments::CommitmentExtractor;
use crate::compaction::TwoPassCompactor;
use crate::goal::GoalStore;
use crate::memory::MemoryBackend;
use crate::messenger::AgentMessenger;
use crate::recall_selector::LlmRecallSelector;
use crate::subagent::SubagentSpawner;
use crate::surfaced::SurfacedStore;
use crate::swarm::SwarmManager;
use crate::todo_gate::TodoGate;
use crate::tools::ToolRegistry;
use crate::types::{RunRequest, RunStream, RuntimeError};
use legion_core::config::{Config, RecallConfig};
use legion_provider::router::ProviderRouter;
use legion_skills::Skill;
use legion_telemetry::TelemetryClient;

/// Strategy for assembling, compacting, and executing the agent context.
///
/// This trait is the PRD R6 hook point: future implementations can replace the
/// legacy hard-coded loop with alternative context management (e.g. a Codex-style
/// engine) without changing `AgentRuntime`.
#[async_trait]
pub trait ContextEngine: Send + Sync {
    /// Stable engine identifier.
    fn id(&self) -> &str;

    /// Execute an agent run and return the event stream.
    ///
    /// The caller supplies the resolved provider router and iteration limit so
    /// that `AgentRuntime` can continue to own per-agent routing.
    fn run(
        &self,
        provider_router: Arc<ProviderRouter>,
        request: RunRequest,
        max_iterations: Option<usize>,
    ) -> Result<RunStream, RuntimeError>;
}

/// The built-in context engine.
///
/// This is a thin wrapper around the existing agent loop; it preserves the
/// current behavior while making the strategy pluggable.
pub struct LegacyContextEngine {
    tool_registry: Arc<dyn ToolRegistry>,
    memory_backend: Arc<dyn MemoryBackend>,
    compactor: Arc<TwoPassCompactor>,
    config: Config,
    plugin_skills: Vec<Skill>,
    auto_extractor: Option<Arc<AutoExtractor>>,
    commitment_extractor: Option<Arc<dyn CommitmentExtractor>>,
    recall_config: RecallConfig,
    selector: Option<Arc<LlmRecallSelector>>,
    surfaced: SurfacedStore,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    messenger: Option<Arc<dyn AgentMessenger>>,
    swarm: Option<Arc<SwarmManager>>,
    todo_gate: TodoGate,
    goal_store: GoalStore,
    telemetry: Option<Arc<TelemetryClient>>,
}

impl LegacyContextEngine {
    pub fn new(
        tool_registry: Arc<dyn ToolRegistry>,
        memory_backend: Arc<dyn MemoryBackend>,
        config: Config,
    ) -> Self {
        let compactor = Arc::new(TwoPassCompactor::new(config.compaction.clone()));
        let recall_config = config.memory.recall.clone();
        let todo_gate = if config.todos.enabled && config.todos.gate.enabled {
            TodoGate::new(config.todos.gate.required_patterns.clone())
        } else {
            TodoGate::default()
        };
        Self {
            tool_registry,
            memory_backend,
            compactor,
            config,
            plugin_skills: Vec::new(),
            auto_extractor: None,
            commitment_extractor: None,
            recall_config,
            selector: None,
            surfaced: SurfacedStore::default(),
            spawner: None,
            messenger: None,
            swarm: None,
            todo_gate,
            goal_store: GoalStore::default(),
            telemetry: None,
        }
    }

    /// Register skills provided by plugins so they are merged into the agent's
    /// skill registry for every run.
    pub fn with_plugin_skills(mut self, skills: Vec<Skill>) -> Self {
        self.plugin_skills = skills;
        self
    }

    /// Attach a background auto-extractor forwarded to each run loop.
    pub fn with_auto_extractor(mut self, extractor: Option<Arc<AutoExtractor>>) -> Self {
        self.auto_extractor = extractor;
        self
    }

    /// Attach a background commitment extractor forwarded to each run loop
    /// (automation-advanced Phase B). `None` disables inferred commitments.
    pub fn with_commitment_extractor(
        mut self,
        extractor: Option<Arc<dyn CommitmentExtractor>>,
    ) -> Self {
        self.commitment_extractor = extractor;
        self
    }

    /// Override the recall configuration forwarded to each run loop (Phase C).
    pub fn with_recall_config(mut self, recall_config: RecallConfig) -> Self {
        self.recall_config = recall_config;
        self
    }

    /// Attach an optional LLM recall re-ranker (Phase C).
    pub fn with_selector(mut self, selector: Option<Arc<LlmRecallSelector>>) -> Self {
        self.selector = selector;
        self
    }

    /// Override the surfaced-ids store used to suppress memories already injected
    /// in earlier turns of the same session (Phase C).
    pub fn with_surfaced(mut self, surfaced: SurfacedStore) -> Self {
        self.surfaced = surfaced;
        self
    }

    /// Override the goal store used for the session-goal gate and context
    /// injection (tests point it at a temp dir).
    pub fn with_goal_store(mut self, goal_store: GoalStore) -> Self {
        self.goal_store = goal_store;
        self
    }

    /// Attach the sub-agent spawner forwarded to each run loop (multi-agent
    /// Phase A). `None` disables `spawn_subagent` (the tool reports unavailable).
    pub fn with_spawner(mut self, spawner: Option<Arc<dyn SubagentSpawner>>) -> Self {
        self.spawner = spawner;
        self
    }

    /// Attach the agent messenger forwarded to each run loop (tools-p1p2
    /// Phase B). `None` disables `agent_to_agent_send` (the tool reports
    /// unavailable).
    pub fn with_messenger(mut self, messenger: Option<Arc<dyn AgentMessenger>>) -> Self {
        self.messenger = messenger;
        self
    }

    /// Attach the swarm manager forwarded to each run loop (multi-agent
    /// Phase D). `None` disables the `swarm_*` tools (they report
    /// unavailable).
    pub fn with_swarm(mut self, swarm: Option<Arc<SwarmManager>>) -> Self {
        self.swarm = swarm;
        self
    }

    /// Attach the telemetry client forwarded to each run loop.
    pub fn with_telemetry(mut self, telemetry: Option<Arc<TelemetryClient>>) -> Self {
        self.telemetry = telemetry;
        self
    }
}

#[async_trait]
impl ContextEngine for LegacyContextEngine {
    fn id(&self) -> &str {
        "legacy"
    }

    fn run(
        &self,
        provider_router: Arc<ProviderRouter>,
        request: RunRequest,
        max_iterations: Option<usize>,
    ) -> Result<RunStream, RuntimeError> {
        use futures::SinkExt;
        use futures::channel::mpsc::channel;

        let ctx = crate::run_loop::RunContext {
            provider_router,
            tool_registry: self.tool_registry.clone(),
            memory_backend: self.memory_backend.clone(),
            compactor: self.compactor.clone(),
            config: self.config.clone(),
            request,
            max_iterations,
            plugin_skills: self.plugin_skills.clone(),
            auto_extractor: self.auto_extractor.clone(),
            commitment_extractor: self.commitment_extractor.clone(),
            recall_config: self.recall_config.clone(),
            selector: self.selector.clone(),
            surfaced: self.surfaced.clone(),
            spawner: self.spawner.clone(),
            messenger: self.messenger.clone(),
            swarm: self.swarm.clone(),
            todo_gate: self.todo_gate.clone(),
            goal_store: self.goal_store.clone(),
            telemetry: self.telemetry.clone(),
        };

        let (mut tx, rx) = channel::<crate::types::RunEvent>(128);

        tokio::spawn(async move {
            if let Err(err) = crate::run_loop::run_loop(ctx, &mut tx).await {
                let _ = tx
                    .send(crate::types::RunEvent::Lifecycle {
                        phase: crate::types::LifecyclePhase::Error,
                        error: Some(err.to_string()),
                    })
                    .await;
                tracing::error!(error = %err, "agent run failed");
            }
        });

        Ok(Box::pin(rx))
    }
}
