//! Runtime assembly for [`AgentHost`](crate::host::AgentHost).
//!
//! This module contains the `AgentHost::new` body and its helper functions.
//! Keeping it separate from the public `host.rs` API keeps the transport-
//! neutral composition root easy to read and lets the Gateway depend only on
//! the public host surface.

use crate::error::HostError;
use crate::metrics::MetricsRegistry;
use crate::routing::Router;
use crate::session::SessionStore;
use crate::system_plugins::load_system_plugins;
use legion_automation::LlmCommitmentExtractor;
use legion_automation::cron::{CronJobStore, JsonlCronJobStore};
use legion_core::config::Config;
use legion_mcp::{McpManager, McpMetrics};
use legion_memory::{Embedder, FakeEmbedder, ProviderEmbedder, SqliteVecBackend};
use legion_provider::auth::load_auth_profiles;
use legion_provider::router::ProviderRouter;
use legion_runtime::{
    AgentRuntime, AutoExtractor, CommitmentExtractor, Harness, HarnessRegistry, LlmRecallSelector,
    MemoryBackend, RuntimeSubagentSpawner, SurfacedStore,
};
use legion_tools::CoreToolRegistry;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

/// Assemble the runtime side from configuration: load and initialize
/// plugins, connect MCP servers, build provider routers, memory, tools,
/// and the harness registry.
pub async fn assemble_agent_host(config: Config) -> Result<super::host::AgentHost, HostError> {
    let mut system_plugins = load_system_plugins().await?;
    // Take the registry out to load user plugins and initialize; the
    // channel provider Arcs stay in `system_plugins` for the caller.
    let mut registry = std::mem::take(&mut system_plugins.registry);
    for plugin_dir in &config.plugins.dirs {
        if let Err(err) = registry.load_dir(plugin_dir, &config.plugins.disabled) {
            tracing::warn!(
                dir = %plugin_dir.display(),
                error = %err,
                "failed to load user plugins from directory"
            );
        }
    }

    let workspace = dirs::home_dir()
        .map(|h| h.join(".legion").join("workspace"))
        .unwrap_or_else(|| PathBuf::from(".legion/workspace"));
    let plugin_ctx = legion_plugin_sdk::PluginContext {
        config: serde_json::Value::Null,
        workspace,
        agent_id: None,
    };
    registry.init_all(&plugin_ctx).await?;
    let plugin_skills = registry.skills().to_vec();

    let metrics = MetricsRegistry::new();

    // Load MCP servers and surface their tools. Servers that fail to
    // connect are recorded and short-circuited so they do not block
    // startup. Tool calls record into the same `MetricsRegistry` that
    // backs `/metrics`.
    let mut mcp_manager = McpManager::new();
    mcp_manager.set_metrics(Arc::new(HostMcpMetrics::new(metrics.clone())));
    if !config.mcp.servers.is_empty() {
        let report = mcp_manager.load(&config.mcp.servers).await;
        if !report.failed.is_empty() {
            tracing::warn!(
                connected = ?report.connected,
                failed = ?report.failed,
                tools = report.tools,
                "loaded MCP servers with failures"
            );
        } else {
            info!(
                connected = ?report.connected,
                tools = report.tools,
                "loaded MCP servers"
            );
        }
    }
    let mcp_manager = Arc::new(mcp_manager);

    let main_router = Arc::new(build_provider_router(&config, "main")?);
    let memory_backend = build_memory_backend(&config).await?;
    let auto_extractor = build_auto_extractor(&config, main_router.clone(), memory_backend.clone());
    // Open the cron store before the runtime so inferred commitments
    // (automation-advanced Phase B) and the cron scheduler share one
    // instance writing to `cron.jsonl`.
    let cron_store: Arc<dyn CronJobStore> = Arc::new(
        JsonlCronJobStore::open(automation_data_dir().join("cron.jsonl"))
            .await
            .map_err(|e| HostError::Automation(format!("cron store: {e}")))?,
    );
    let commitment_extractor =
        build_commitment_extractor(&config, main_router.clone(), cron_store.clone());
    let recall_selector = build_recall_selector(&config, main_router.clone());
    let session_store = Arc::new(SessionStore::default());
    let mut core_tools = CoreToolRegistry::new_with_mcp(&config, Some(mcp_manager.tools()));
    // Session self-inspection tools (tools-p1p2 Phase A). Read-only, so
    // they default to Approval::Off. Permission boundary (gap doc §6.6):
    // each tool only ever accesses sessions of ctx.agent_id; cross-agent
    // reads are rejected inside the tools.
    core_tools.register(Arc::new(crate::session_tools::SessionStatusTool::new(
        session_store.clone(),
        legion_runtime::tools::Policy::from_config(
            config.tools.get("session_status"),
            legion_runtime::tools::Approval::Off,
        ),
    )));
    core_tools.register(Arc::new(crate::session_tools::SessionsListTool::new(
        session_store.clone(),
        legion_runtime::tools::Policy::from_config(
            config.tools.get("sessions_list"),
            legion_runtime::tools::Approval::Off,
        ),
        config.sessions.lite_read_buffer_bytes,
    )));
    core_tools.register(Arc::new(crate::session_tools::SessionsHistoryTool::new(
        session_store.clone(),
        legion_runtime::tools::Policy::from_config(
            config.tools.get("sessions_history"),
            legion_runtime::tools::Approval::Off,
        ),
    )));
    // image_generate (tools-p1p2 Phase B). Defaults to Approval::Required:
    // generation costs money and carries content risk (gap doc §4.3/§6.1).
    // Uses the main agent's router; per-model selection is a tool input.
    core_tools.register(Arc::new(crate::image_tool::ImageGenerateTool::new(
        main_router.clone(),
        legion_runtime::tools::Policy::from_config(
            config.tools.get("image_generate"),
            legion_runtime::tools::Approval::Required,
        ),
    )));
    // tts (tools-p1p2 Phase C). Defaults to Approval::Off: speech
    // synthesis is low-risk (gap doc §4.5). Voice channel delivery /
    // capabilities gating is a later slice; the tool writes the audio
    // file under <workspace>/generated/ and returns its path for now.
    core_tools.register(Arc::new(crate::tts_tool::TtsTool::new(
        main_router.clone(),
        legion_runtime::tools::Policy::from_config(
            config.tools.get("tts"),
            legion_runtime::tools::Approval::Off,
        ),
    )));
    let mut agent_runtime = AgentRuntime::new(
        main_router,
        Arc::new(core_tools),
        memory_backend,
        config.clone(),
    )
    .with_plugin_skills(plugin_skills)
    .with_auto_extractor(auto_extractor)
    .with_commitment_extractor(commitment_extractor)
    .with_selector(recall_selector)
    .with_surfaced(SurfacedStore::default());
    for agent in &config.agents.list {
        agent_runtime = agent_runtime.with_agent_router(
            agent.id.clone(),
            Arc::new(build_provider_router(&config, &agent.id)?),
        );
    }
    let mut harness_registry = HarnessRegistry::new();
    if let Some(id) = config.agent_runtime.id.clone() {
        harness_registry = harness_registry.with_default(id);
    }
    let agent_runtime = Arc::new(agent_runtime);
    let spawner = Arc::new(RuntimeSubagentSpawner::new(
        agent_runtime.clone(),
        config.subagents.clone(),
    ));
    agent_runtime.set_spawner(spawner.clone());
    // Swarm teammates (multi-agent Phase D): in-process named teammates
    // driven by mailboxes; each teammate turn runs through the shared
    // sub-agent spawner. Same late-binding pattern as the spawner.
    agent_runtime.set_swarm(Arc::new(legion_runtime::SwarmManager::new(spawner)));
    // Agent-to-agent messenger (tools-p1p2 Phase B): same late-binding
    // pattern as the spawner — it needs the fully-built runtime.
    agent_runtime.set_messenger(Arc::new(
        crate::agent_messenger::RuntimeAgentMessenger::new(agent_runtime.clone(), config.clone()),
    ));
    harness_registry.register(agent_runtime);
    if let Some(command) = config.acp.command.clone() {
        harness_registry.register(Arc::new(legion_acp::AcpHarness::new(
            command,
            Arc::new(CoreToolRegistry::new(&config)),
            config.clone(),
        )));
    }
    let runtime: Arc<dyn Harness> = Arc::new(harness_registry);
    let router = Router::from_config(&config);

    Ok(super::host::AgentHost {
        config,
        session_store,
        runtime,
        router,
        cron_store,
        metrics,
        mcp_manager,
        registry: Arc::new(registry),
        system_plugins,
    })
}

/// Bridges [`McpMetrics`] to the host [`MetricsRegistry`] so MCP tool calls
/// surface as labeled counters for observability consumers.
struct HostMcpMetrics {
    registry: MetricsRegistry,
}

impl HostMcpMetrics {
    fn new(registry: MetricsRegistry) -> Self {
        Self { registry }
    }
}

impl McpMetrics for HostMcpMetrics {
    fn record_call(&self, server: &str, tool: &str) {
        self.registry.increment_counter_with_labels(
            "mcp_calls_total",
            "total mcp tool calls",
            &[
                ("server".to_string(), server.to_string()),
                ("tool".to_string(), tool.to_string()),
            ],
        );
    }

    fn record_error(&self, server: &str, tool: &str) {
        self.registry.increment_counter_with_labels(
            "mcp_errors_total",
            "total mcp tool errors",
            &[
                ("server".to_string(), server.to_string()),
                ("tool".to_string(), tool.to_string()),
            ],
        );
    }
}

pub(crate) fn build_provider_router(
    config: &Config,
    agent_id: &str,
) -> Result<ProviderRouter, HostError> {
    let auth_profiles = load_auth_profiles(agent_id).unwrap_or_default();
    let costs_path =
        dirs::home_dir().map(|h| h.join(format!(".legion/agents/{agent_id}/costs.json")));
    let mut router = ProviderRouter::from_configs(
        &config.models.providers,
        &auth_profiles,
        &config.models.costs,
        costs_path,
    )
    .map_err(|e| HostError::Runtime(format!("provider router: {e}")))?;
    router.set_aliases(config.models.aliases.clone());
    router.set_fallbacks(config.models.fallbacks.clone());
    Ok(router)
}

pub(crate) async fn build_memory_backend(
    config: &Config,
) -> Result<Arc<dyn MemoryBackend>, HostError> {
    let collection_path = config
        .memory
        .builtin
        .collection_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".legion").join("memory")))
        .unwrap_or_else(|| PathBuf::from(".legion/memory"));

    let workspace = legion_runtime::resolve_workspace(config, "main", None);
    let dimension = config.memory.builtin.embedding_dimension;

    let embedder: Arc<dyn Embedder> = match &config.memory.builtin.embedding_provider {
        Some(model_ref) => {
            let router = Arc::new(build_provider_router(config, "main")?);
            Arc::new(ProviderEmbedder::new(router, model_ref, dimension))
        }
        None => Arc::new(FakeEmbedder::new(dimension)),
    };

    let backend = SqliteVecBackend::open(&collection_path, &workspace, embedder)
        .await
        .map_err(|e| HostError::Runtime(format!("memory backend: {e}")))?
        .with_decay_config(config.memory.decay.clone())
        .with_merge_config(config.memory.merge.clone());

    // Index MEMORY.md on startup if it exists.
    let memory_md = workspace.join("MEMORY.md");
    if memory_md.exists() {
        if let Err(e) = backend.index_file(&memory_md).await {
            tracing::warn!("failed to index MEMORY.md: {}", e);
        } else {
            tracing::info!("indexed MEMORY.md at {}", memory_md.display());
        }
    }

    Ok(Arc::new(backend))
}

/// Build the background auto-extractor when `memory.autoExtract` is enabled and a
/// model is configured. Returns `None` (memory stays manual) otherwise. An
/// enabled-but-model-less config is warned about and treated as disabled.
pub(crate) fn build_auto_extractor(
    config: &Config,
    router: Arc<ProviderRouter>,
    memory: Arc<dyn MemoryBackend>,
) -> Option<Arc<AutoExtractor>> {
    let cfg = &config.memory.auto_extract;
    if !cfg.enabled {
        return None;
    }
    let Some(model) = cfg.model.clone() else {
        tracing::warn!(
            "memory.autoExtract.enabled is true but no model is configured; auto-extract disabled"
        );
        return None;
    };
    Some(Arc::new(AutoExtractor::new(
        router,
        model,
        memory,
        cfg.max_messages,
        cfg.max_facts_per_turn,
        cfg.cooldown_seconds,
        std::time::Duration::from_secs(cfg.timeout_seconds),
    )))
}

/// Build the background commitment extractor when `commitments.enabled` is set
/// and a model is configured (automation-advanced Phase B). Returns `None`
/// (inference disabled) otherwise. An enabled-but-model-less config is warned
/// about and treated as disabled.
pub(crate) fn build_commitment_extractor(
    config: &Config,
    router: Arc<ProviderRouter>,
    store: Arc<dyn CronJobStore>,
) -> Option<Arc<dyn CommitmentExtractor>> {
    let cfg = &config.commitments;
    if !cfg.enabled {
        return None;
    }
    let Some(model) = cfg.model.clone() else {
        tracing::warn!(
            "commitments.enabled is true but no model is configured; commitment inference disabled"
        );
        return None;
    };
    Some(Arc::new(LlmCommitmentExtractor::new(
        router,
        model,
        store,
        cfg.max_messages,
        cfg.max_per_turn,
        cfg.cooldown_seconds,
        std::time::Duration::from_secs(cfg.timeout_seconds),
    )))
}

/// Default automation data directory (`~/.legion/automation`).
pub fn automation_data_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".legion").join("automation"))
        .unwrap_or_else(|| PathBuf::from(".legion/automation"))
}

/// Build the optional LLM recall re-ranker when `memory.recall.useLlmSelector`
/// is enabled and a `selectorModel` is configured. Returns `None` (recall keeps
/// backend ranking) otherwise. An enabled-but-model-less config is warned about
/// and treated as disabled.
pub(crate) fn build_recall_selector(
    config: &Config,
    router: Arc<ProviderRouter>,
) -> Option<Arc<LlmRecallSelector>> {
    let cfg = &config.memory.recall;
    if !cfg.use_llm_selector {
        return None;
    }
    let Some(model) = cfg.selector_model.clone() else {
        tracing::warn!(
            "memory.recall.useLlmSelector is true but no selectorModel is configured; \
             LLM recall selector disabled"
        );
        return None;
    };
    Some(Arc::new(LlmRecallSelector::new(
        router,
        model,
        std::time::Duration::from_secs(15),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricValue;
    use tempfile::TempDir;

    #[tokio::test]
    async fn build_memory_backend_creates_sqlite_backend_and_indexes_memory_md() {
        let tmp = TempDir::new().unwrap();
        let collection_path = tmp.path().join("memory");
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::write(workspace.join("MEMORY.md"), "User prefers dark mode.")
            .await
            .unwrap();

        let config = Config::from_json(&format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{ "defaults": {{ "workspace": "{}" }} }},
                "memory": {{
                    "builtin": {{
                        "collectionPath": "{}",
                        "embeddingDimension": 64
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/"),
            collection_path.display().to_string().replace('\\', "/"),
        ))
        .unwrap();

        let backend = build_memory_backend(&config).await.unwrap();

        // The backend should be able to search the indexed MEMORY.md.
        let notes = backend.search("dark mode", 5).await.unwrap();
        assert!(
            notes.iter().any(|n| n.content.contains("dark mode")),
            "expected MEMORY.md content to be searchable"
        );
    }

    #[test]
    fn host_mcp_metrics_writes_labeled_counters() {
        let registry = MetricsRegistry::new();
        let sink = HostMcpMetrics::new(registry.clone());
        sink.record_call("fs", "read_file");
        sink.record_call("fs", "read_file");
        sink.record_error("fs", "write_file");

        let snapshot = registry.snapshot();
        let calls = snapshot
            .iter()
            .find(|m| m.name == "mcp_calls_total")
            .expect("mcp_calls_total present");
        assert_eq!(calls.value, MetricValue::Counter(2));
        assert!(calls.labels.iter().any(|(k, v)| k == "server" && v == "fs"));
        assert!(
            calls
                .labels
                .iter()
                .any(|(k, v)| k == "tool" && v == "read_file")
        );

        let errors = snapshot
            .iter()
            .find(|m| m.name == "mcp_errors_total")
            .expect("mcp_errors_total present");
        assert_eq!(errors.value, MetricValue::Counter(1));
    }
}
