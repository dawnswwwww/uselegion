//! Agent runtime assembly shared by the Gateway and embedded hosts.
//!
//! [`AgentHost`] owns the runtime side of the gateway: system + user plugins,
//! provider routers, the memory backend, MCP tools, the core tool registry,
//! the harness registry, the session transcript store, and the shared cron
//! store. The Gateway builds the distribution layer (channel provider
//! startup, the HTTP/WS server, automation loops) on top of it; the embedded
//! CLI mode drives the same host without any of that.

use crate::error::HostError;
use crate::metrics::MetricsRegistry;
use crate::routing::Router;
use crate::session::SessionStore;
use crate::system_plugins::SystemPlugins;
use crate::turn::prepare_run;
use legion_automation::cron::CronJobStore;
use legion_core::config::Config;
use legion_mcp::McpManager;
use legion_plugin_sdk::PluginRegistry;
use legion_protocol::{AgentAccepted, AgentParams};
use legion_runtime::Harness;
use legion_runtime::RunStream;
use std::sync::Arc;

/// Runtime-side components assembled from a [`Config`].
///
/// Everything an agent turn needs that is independent of the transport:
/// the harness registry, session transcripts, agent routing, the shared cron
/// store, metrics, MCP connections, and the initialized plugin registry.
pub struct AgentHost {
    /// Effective configuration the host was assembled from.
    pub config: Config,
    /// Persistent transcript store per session key.
    pub session_store: Arc<SessionStore>,
    /// Fully assembled harness registry (built-in runtime + optional ACP).
    pub runtime: Arc<dyn Harness>,
    /// Agent binding router (session-key / inbound agent resolution).
    pub router: Router,
    /// Shared cron job store. Scheduling lives in the Gateway, but inferred
    /// commitments inside the runtime write to the same store.
    pub cron_store: Arc<dyn CronJobStore>,
    /// Metrics sink shared with MCP call accounting and the `/metrics` route.
    pub metrics: MetricsRegistry,
    /// Connected MCP servers; call `shutdown_all` on host teardown.
    pub mcp_manager: Arc<McpManager>,
    /// Initialized plugin registry (channels, skills, capabilities).
    pub registry: Arc<PluginRegistry>,
    /// Loaded (not yet started) built-in channel providers. Starting them is
    /// the distribution layer's job (the Gateway).
    pub system_plugins: SystemPlugins,
}

impl AgentHost {
    /// Assemble the runtime side from configuration: load and initialize
    /// plugins, connect MCP servers, build provider routers, memory, tools,
    /// and the harness registry.
    pub async fn new(config: Config) -> Result<Self, HostError> {
        crate::assembly::assemble_agent_host(config, None).await
    }

    /// Assemble the runtime side with an explicit cron store path for the
    /// scheduler tools. `None` keeps the default `~/.legion/automation/cron.jsonl`.
    pub async fn new_with_cron_store_path(
        config: Config,
        cron_store_path: Option<std::path::PathBuf>,
    ) -> Result<Self, HostError> {
        crate::assembly::assemble_agent_host(config, cron_store_path).await
    }

    /// Resolve the session key, load + repair resumable history, and start an
    /// agent run. Returns the run stream, the accepted metadata, and the
    /// resolved session key.
    ///
    /// `approval_gate` is attached to the run's [`RunRequest`](legion_runtime::RunRequest)
    /// so interactive approval prompts can reach the caller; pass `None` to
    /// fall back to the runtime's no-op (timeout-only) gate.
    pub async fn prepare_run(
        &self,
        params: AgentParams,
        approval_gate: Option<Arc<legion_runtime::ApprovalGate>>,
        question_gate: Option<Arc<legion_runtime::QuestionGate>>,
    ) -> Result<(RunStream, AgentAccepted, String), String> {
        prepare_run(
            &*self.runtime,
            &self.config,
            &self.router,
            &self.session_store,
            params,
            approval_gate,
            question_gate,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_provider::types::ChatMessage as ProviderChatMessage;
    use tempfile::TempDir;

    #[tokio::test]
    async fn agent_host_new_assembles_runtime_components() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let collection_path = tmp.path().join("memory");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

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

        let host = AgentHost::new(config).await.unwrap();

        // Harness registry assembled with the built-in runtime.
        assert_eq!(host.runtime.id(), "registry");
        assert!(host.runtime.can_handle("openai/gpt-4o"));

        // Router resolves the default agent for an unbound channel.
        let inbound = legion_channel::webchat_inbound("u1".to_string(), "hi".to_string());
        assert_eq!(host.router.resolve_agent(&inbound), "main");

        // Session store round-trips a transcript. The store is rooted in the
        // user's home dir, so assert relative to the pre-existing length to
        // keep the test idempotent across runs.
        let key = "agent:main:dm:webchat:default:direct:agent-host-smoke";
        let before = host.session_store.load_for_resume(key).await.len();
        host.session_store
            .append(key, &[ProviderChatMessage::user("hello".to_string())])
            .await
            .unwrap();
        let history = host.session_store.load_for_resume(key).await;
        assert_eq!(history.len(), before + 1);
        assert_eq!(history.last().map(|m| m.content.as_str()), Some("hello"));

        // Plugin registry carries the system channel plugins.
        assert!(
            host.registry
                .list()
                .iter()
                .any(|p| p.metadata().id == "system:channel-webchat")
        );
    }
}
