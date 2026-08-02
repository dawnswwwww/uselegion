use crate::error::GatewayError;
use crate::events::EventBus;
use crate::events::events_handler;
use crate::http::{canvas_placeholder, dashboard, webhook_handler};
use crate::market::PluginMarket;
use crate::nodes::NodeManager;
use crate::observability::metrics_handler;
use crate::pairing::PairingStore;
use crate::websocket::{GatewayState, websocket_handler};
use axum::routing::{get, post};
use axum::{Extension, Router as AxumRouter};
use legion_automation::cron::{self, CronJobStore, CronScheduler};
use legion_automation::heartbeat::{Heartbeat, HeartbeatConfig};
use legion_automation::task_runner::TaskRunner;
use legion_automation::tasks::JsonlTaskStore;
use legion_channel::{TelegramProvider, WebChatProvider};
use legion_core::config::Config;
use legion_host::{AgentHost, MetricsRegistry, SessionStore, routing::Router};
use legion_mcp::McpManager;
use legion_plugin_sdk::PluginRegistry;
use legion_plugin_sdk::channel::{ChannelProvider, InboundMessage};
use legion_provider::model_ref::resolve_agent_model;
use legion_runtime::{AgentRuntime, ApprovalQueueRegistry, Harness};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tracing::info;

/// The central Gateway process.
pub struct Gateway {
    config: Config,
    registry: Arc<PluginRegistry>,
    runtime: Arc<dyn Harness>,
    pairing_store: PairingStore,
    shutdown_tx: Option<oneshot::Sender<()>>,
    gateway_id: String,
    webchat: Arc<WebChatProvider>,
    _telegram: Arc<TelegramProvider>,
    _slack: Arc<legion_channel::SlackProvider>,
    _discord: Arc<legion_channel::DiscordProvider>,
    _lark: Arc<legion_channel::LarkProvider>,
    _matrix: Arc<legion_channel::MatrixProvider>,
    _inbound_router_handle: Option<tokio::task::JoinHandle<()>>,
    _automation_handles: Vec<tokio::task::JoinHandle<()>>,
    cron_scheduler: Option<Arc<CronScheduler>>,
    task_store: Option<legion_automation::tasks::SharedTaskStore>,
    task_runner: Option<Arc<TaskRunner>>,
    /// Shared cron job store handed to the automation subsystem when it is
    /// started after a successful bind.
    cron_store: Arc<dyn CronJobStore>,
    node_manager: Arc<NodeManager>,
    metrics_registry: MetricsRegistry,
    plugin_market: PluginMarket,
    session_store: Arc<SessionStore>,
    approval_registry: Arc<ApprovalQueueRegistry>,
    question_registry: Arc<legion_runtime::QuestionQueueRegistry>,
    mcp_manager: Arc<McpManager>,
}

impl Gateway {
    /// Build a new Gateway from configuration: assemble the runtime side via
    /// [`AgentHost`](legion_host::AgentHost), then start the built-in channel
    /// providers and the distribution layer (inbound router, automation,
    /// nodes, market).
    pub async fn new(config: Config) -> Result<Self, GatewayError> {
        let host = AgentHost::new(config).await.map_err(|e| match e {
            legion_host::HostError::Config(err) => GatewayError::Config(err),
            legion_host::HostError::Plugin(err) => GatewayError::Plugin(err),
            legion_host::HostError::Channel(err) => GatewayError::Channel(err),
            legion_host::HostError::Runtime(msg) => GatewayError::Runtime(msg),
            legion_host::HostError::Io(err) => GatewayError::Io(err),
            legion_host::HostError::Automation(msg) => GatewayError::Automation(msg),
        })?;
        let config = host.config.clone();
        let registry = host.registry.clone();
        let metrics_registry = host.metrics.clone();
        let mcp_manager = host.mcp_manager.clone();
        let session_store = host.session_store.clone();
        let question_registry = Arc::new(legion_runtime::QuestionQueueRegistry::new());
        let runtime = host.runtime.clone();
        let cron_store = host.cron_store.clone();
        let webchat = host.system_plugins.webchat.clone();
        let telegram = host.system_plugins.telegram.clone();
        let slack = host.system_plugins.slack.clone();
        let discord = host.system_plugins.discord.clone();
        let lark = host.system_plugins.lark.clone();
        let matrix = host.system_plugins.matrix.clone();

        // Channel inbound router.
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(256);
        let router_registry = registry.clone();
        let router_runtime = runtime.clone();
        let router_config = config.clone();
        let approval_registry = Arc::new(ApprovalQueueRegistry::new());
        let router_approval_registry = approval_registry.clone();
        let inbound_router = Arc::new(host.router.clone());
        let router_session_store = session_store.clone();
        let bot_guard = Arc::new(legion_channel::access::BotLoopGuard::new(
            std::time::Duration::from_secs(60),
            5,
        ));
        let inbound_router_handle = tokio::spawn(async move {
            while let Some(msg) = inbound_rx.recv().await {
                legion_host::channel_inbound::route_inbound_to_runtime(
                    router_runtime.clone(),
                    router_config.clone(),
                    inbound_router.clone(),
                    router_session_store.clone(),
                    router_registry.clone(),
                    Some(router_approval_registry.clone()),
                    Some(bot_guard.clone()),
                    msg,
                )
                .await;
            }
        });

        // Start WebChat provider (always available for the Web UI).
        webchat
            .start(serde_json::Value::Null, inbound_tx.clone())
            .await?;

        // Start Telegram provider only when a token is configured.
        if let Some(telegram_config) = config.channels.get("telegram") {
            telegram
                .start(telegram_config.clone(), inbound_tx.clone())
                .await?;
        }

        // Start Slack provider when botToken + appToken are configured.
        if let Some(slack_config) = config.channels.get("slack") {
            slack
                .start(slack_config.clone(), inbound_tx.clone())
                .await?;
        }

        // Start Discord provider when a botToken is configured.
        if let Some(discord_config) = config.channels.get("discord") {
            discord
                .start(discord_config.clone(), inbound_tx.clone())
                .await?;
        }

        // Start Lark provider when appId + appSecret are configured.
        if let Some(lark_config) = config.channels.get("lark") {
            lark.start(lark_config.clone(), inbound_tx.clone()).await?;
        }

        // Start Matrix provider when homeserver + accessToken are configured.
        if let Some(matrix_config) = config.channels.get("matrix") {
            matrix.start(matrix_config.clone(), inbound_tx).await?;
        }

        // Automation (cron scheduler, heartbeat, hooks, task runner) is NOT
        // started here: a process that fails to bind must never run scheduled
        // jobs. It starts after a successful bind in start()/start_bound(), or
        // explicitly via start_automation() from tests that serve router()
        // directly.
        let node_manager = Arc::new(NodeManager::new());
        let plugin_market = PluginMarket::new().with_system_plugins();

        Ok(Self {
            config,
            registry,
            runtime,
            pairing_store: PairingStore::new(),
            shutdown_tx: None,
            gateway_id: format!("gw-{}", legion_core::util::next_id()),
            webchat,
            _telegram: telegram,
            _slack: slack,
            _discord: discord,
            _lark: lark,
            _matrix: matrix,
            _inbound_router_handle: Some(inbound_router_handle),
            _automation_handles: Vec::new(),
            cron_scheduler: None,
            task_store: None,
            task_runner: None,
            cron_store,
            node_manager,
            metrics_registry,
            plugin_market,
            session_store,
            approval_registry,
            question_registry,
            mcp_manager,
        })
    }

    /// Replace the runtime with a caller-provided one (useful for tests).
    pub fn with_runtime(mut self, runtime: Arc<AgentRuntime>) -> Self {
        self.runtime = runtime.clone();
        self.task_runner = self.task_runner.take().map(|tr| {
            Arc::new(TaskRunner::new(
                tr.task_store.clone(),
                runtime.clone(),
                tr.config.clone(),
            ))
        });
        self
    }

    /// Replace the session store with a caller-provided one (useful for tests).
    pub fn with_session_store(mut self, store: Arc<SessionStore>) -> Self {
        self.session_store = store;
        self
    }

    /// Return a reference to the loaded plugin registry.
    pub fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    /// Return a handle to the shared approval-queue registry.
    pub fn approval_registry(&self) -> Arc<ApprovalQueueRegistry> {
        self.approval_registry.clone()
    }

    /// Start the automation subsystem (cron scheduler, heartbeat, hooks, task
    /// runner). Must only run after the listener has been bound: a duplicate
    /// gateway process that fails to bind would otherwise keep executing
    /// scheduled jobs and leave orphaned task records behind. Idempotent.
    pub async fn start_automation(&mut self) -> Result<(), GatewayError> {
        if self.cron_scheduler.is_some() {
            return Ok(());
        }
        let (handles, scheduler, task_store, task_runner) =
            start_automation(&self.config, self.runtime.clone(), self.cron_store.clone()).await?;
        self._automation_handles = handles;
        self.cron_scheduler = scheduler;
        self.task_store = task_store;
        self.task_runner = task_runner;
        Ok(())
    }

    /// Start the WS/HTTP server. This method consumes `self` and blocks until shutdown.
    pub async fn start(mut self) -> Result<(), GatewayError> {
        self.archive_expired_sessions().await;
        let listener = self.bind_listener().await?;
        // Only start scheduled work once the port is ours; otherwise every
        // failed duplicate-start attempt would also run cron jobs.
        self.start_automation().await?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);
        self.run_server(listener, shutdown_rx).await
    }

    /// Start the WS/HTTP server in a background task, binding to the configured
    /// address. Port 0 may be used to obtain an ephemeral port; the returned
    /// address reflects the actual bound port.
    ///
    /// The caller can signal graceful shutdown by sending on the returned
    /// oneshot channel.
    pub async fn start_bound(
        mut self,
    ) -> Result<
        (
            SocketAddr,
            tokio::task::JoinHandle<Result<(), GatewayError>>,
            oneshot::Sender<()>,
        ),
        GatewayError,
    > {
        self.archive_expired_sessions().await;
        let listener = self.bind_listener().await?;
        // Only start scheduled work once the port is ours; otherwise every
        // failed duplicate-start attempt would also run cron jobs.
        self.start_automation().await?;
        let bound_addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let handle = tokio::spawn(async move { self.run_server(listener, shutdown_rx).await });

        info!(%bound_addr, "Legion Gateway started in background");
        Ok((bound_addr, handle, shutdown_tx))
    }

    /// One-shot TTL archival at startup (session-resume Phase C). Disabled
    /// unless `sessions.ttlDays > 0`; archiving moves transcripts to
    /// `sessions.archiveDir` instead of deleting them.
    async fn archive_expired_sessions(&self) {
        let ttl = self.config.sessions.ttl_days;
        if ttl == 0 {
            return;
        }
        let raw = &self.config.sessions.archive_dir;
        let archive_dir = raw
            .strip_prefix("~/")
            .and_then(|rest| dirs::home_dir().map(|h| h.join(rest)))
            .unwrap_or_else(|| PathBuf::from(raw));
        let archived = self.session_store.archive_expired(ttl, &archive_dir).await;
        if !archived.is_empty() {
            info!(
                count = archived.len(),
                archive_dir = %archive_dir.display(),
                "archived expired session transcripts"
            );
        }
    }

    async fn bind_listener(&self) -> Result<TcpListener, GatewayError> {
        let addr: SocketAddr = format!(
            "{}:{}",
            self.config.gateway.bind_host, self.config.gateway.port
        )
        .parse()
        .map_err(|e| GatewayError::Server(format!("invalid bind address: {e}")))?;

        info!(%addr, "Legion Gateway binding");
        TcpListener::bind(&addr).await.map_err(GatewayError::Io)
    }

    /// Run the axum server on the provided listener until the shutdown signal fires.
    async fn run_server(
        self,
        listener: TcpListener,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), GatewayError> {
        let server = axum::serve(listener, self.router());
        let result = tokio::select! {
            result = server => result.map_err(|e| GatewayError::Server(e.to_string())),
            _ = shutdown_rx => {
                info!("Legion Gateway shutdown signal received");
                Ok(())
            }
        };

        // Stop background automation and inbound routing so a shutting-down
        // gateway does not keep running cron jobs or processing channel messages.
        for handle in self._automation_handles {
            handle.abort();
        }
        if let Some(handle) = self._inbound_router_handle {
            handle.abort();
        }

        if let Err(err) = self.webchat.stop().await {
            tracing::warn!(error = %err, "failed to stop webchat channel");
        }
        if self.config.channels.contains_key("telegram") {
            if let Err(err) = self._telegram.stop().await {
                tracing::warn!(error = %err, "failed to stop telegram channel");
            }
        }
        if self.config.channels.contains_key("slack") {
            if let Err(err) = self._slack.stop().await {
                tracing::warn!(error = %err, "failed to stop slack channel");
            }
        }
        if self.config.channels.contains_key("discord") {
            if let Err(err) = self._discord.stop().await {
                tracing::warn!(error = %err, "failed to stop discord channel");
            }
        }
        if self.config.channels.contains_key("lark") {
            if let Err(err) = self._lark.stop().await {
                tracing::warn!(error = %err, "failed to stop lark channel");
            }
        }
        if self.config.channels.contains_key("matrix") {
            if let Err(err) = self._matrix.stop().await {
                tracing::warn!(error = %err, "failed to stop matrix channel");
            }
        }
        self.mcp_manager.shutdown_all().await;

        result
    }

    /// Build the axum router for this Gateway (exposed for integration tests).
    pub fn router(&self) -> AxumRouter {
        let state = Arc::new(GatewayState {
            config: self.config.clone(),
            pairing_store: self.pairing_store.clone(),
            runtime: self.runtime.clone(),
            router: Router::from_config(&self.config),
            gateway_id: self.gateway_id.clone(),
            webchat: self.webchat.clone(),
            registry: self.registry.clone(),
            cron_scheduler: self.cron_scheduler.clone(),
            task_store: self.task_store.clone(),
            task_runner: self.task_runner.clone(),
            node_manager: self.node_manager.clone(),
            metrics_registry: self.metrics_registry.clone(),
            plugin_market: self.plugin_market.clone(),
            session_store: self.session_store.clone(),
            approval_registry: self.approval_registry.clone(),
            question_registry: self.question_registry.clone(),
            event_bus: EventBus::new(),
        });

        let mut router = AxumRouter::new()
            .route("/", get(dashboard))
            .route("/dashboard", get(dashboard))
            .route(
                "/dashboard/assets/dashboard.js",
                get(legion_web::dashboard_js),
            )
            .route("/__legion__/canvas/", get(canvas_placeholder))
            .route("/webhook/{id}", post(webhook_handler))
            .route("/ws", get(websocket_handler))
            .route("/events", get(events_handler));

        if self.config.observability.enabled && self.config.observability.metrics_enabled {
            router = router.route(
                &self.config.observability.metrics_path,
                get(metrics_handler),
            );
        }

        router.layer(Extension(state))
    }

    /// Trigger a graceful shutdown. This consumes `self` because the server owns the sender.
    pub fn shutdown(self) -> Result<(), GatewayError> {
        if let Some(tx) = self.shutdown_tx {
            let _ = tx.send(());
        }
        Ok(())
    }
}

async fn start_automation(
    config: &Config,
    runtime: Arc<dyn Harness>,
    cron_store: Arc<dyn CronJobStore>,
) -> Result<
    (
        Vec<tokio::task::JoinHandle<()>>,
        Option<Arc<CronScheduler>>,
        Option<legion_automation::tasks::SharedTaskStore>,
        Option<Arc<TaskRunner>>,
    ),
    GatewayError,
> {
    let mut handles = Vec::new();

    let data_dir = legion_host::assembly::automation_data_dir();

    let task_store: legion_automation::tasks::SharedTaskStore = Arc::new(
        JsonlTaskStore::open(data_dir.join("tasks.jsonl"))
            .await
            .map_err(|e| GatewayError::Automation(format!("task store: {e}")))?,
    );

    let scheduler = Arc::new(CronScheduler::new(
        cron_store,
        task_store.clone(),
        runtime.clone(),
        config.clone(),
    ));
    handles.push(tokio::spawn(cron::cron_loop(scheduler.clone())));

    let task_runner = Arc::new(TaskRunner::new(
        task_store.clone(),
        runtime.clone(),
        config.clone(),
    ));
    handles.push(tokio::spawn(
        task_runner
            .clone()
            .background_loop(std::time::Duration::from_secs(30)),
    ));

    if config.heartbeat.enabled {
        let workspace = legion_runtime::resolve_workspace(config, "main", None);
        let model_ref = resolve_agent_model(config, "main");
        let heartbeat = Arc::new(Heartbeat::new(
            HeartbeatConfig {
                agent_id: "main".to_string(),
                interval_minutes: config.heartbeat.interval_minutes,
                workspace,
            },
            runtime,
            model_ref,
        ));
        handles.push(tokio::spawn(heartbeat.run()));
    }

    Ok((
        handles,
        Some(scheduler),
        Some(task_store),
        Some(task_runner),
    ))
}
