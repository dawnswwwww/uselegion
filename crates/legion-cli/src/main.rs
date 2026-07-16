use clap::{Parser, Subcommand};
use legion_cli::{
    CliError, DEFAULT_CLI_SESSION_KEY, GatewayClient, config_get, config_set, default_config_path,
    default_manifest_url, doctor, driver::CliMode, gateway_logs, gateway_manager::GatewayManager,
    load_config, resolve_session_key_arg, setup::SetupOptions, show_context,
    start_gateway_with_options, stop_gateway, validate_config,
};
use legion_core::config::Config;
use legion_memory::{Embedder, FakeEmbedder, ProviderEmbedder, SqliteVecBackend};
use legion_provider::auth::load_auth_profiles;
use legion_provider::router::ProviderRouter;
use legion_runtime::{MemoryBackend, resolve_workspace};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "legion")]
#[command(about = "Legion agent harness CLI", long_about = None)]
struct Cli {
    /// Resume a specific TUI session: a peer id (transcript file name under
    /// `~/.legion/agents/<agent>/sessions/`) or a full `agent:...` session
    /// key. Without it each TUI launch starts a fresh session.
    #[arg(long)]
    session: Option<String>,
    /// Run the TUI with the agent runtime embedded in this process. This is
    /// the default — the flag is kept as an explicit no-op for clarity.
    #[arg(long, conflicts_with = "gateway")]
    local: bool,
    /// Require the gateway for the TUI (started if needed); never run
    /// embedded. Use this to share a long-running runtime across windows or
    /// with remote channels.
    #[arg(long)]
    gateway: bool,
    /// Yolo mode: auto-approve every tool prompt without asking. Hard
    /// policy denies (e.g. disabled tools) still apply.
    #[arg(long)]
    yolo: bool,
    /// Working directory for the agent. Defaults to the current directory.
    /// Pass an explicit path to override; pass `none` to use the config
    /// default (~/.legion/workspace, i.e. the legacy behavior).
    #[arg(long, value_name = "PATH|none")]
    workspace: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start, stop, or check the Gateway.
    Gateway {
        #[command(subcommand)]
        action: GatewayAction,
    },
    /// Send a single agent turn and print the reply (via the gateway when
    /// reachable, otherwise embedded in this process).
    Agent {
        /// Dump the assembled system prompt for this run to
        /// `~/.legion/dump-prompts/<session>.jsonl`.
        #[arg(long)]
        dump_prompts: bool,
        /// Resume a specific session: a peer id (transcript file name under
        /// `~/.legion/agents/<agent>/sessions/`) or a full `agent:...`
        /// session key. Without it the shared `cli` session is used.
        #[arg(long)]
        session: Option<String>,
        /// Run the turn with the agent runtime embedded in this process.
        /// This is the default — the flag is kept as an explicit no-op.
        #[arg(long, conflicts_with = "gateway")]
        local: bool,
        /// Require the gateway for this turn; never run embedded.
        #[arg(long)]
        gateway: bool,
        /// Yolo mode: auto-approve every tool prompt without asking. Hard
        /// policy denies (e.g. disabled tools) still apply.
        #[arg(long)]
        yolo: bool,
        /// Working directory for this turn. Defaults to the current
        /// directory. Pass `none` to use the config default
        /// (~/.legion/workspace).
        #[arg(long, value_name = "PATH|none")]
        workspace: Option<String>,
        message: Vec<String>,
    },
    /// Show the latest prompt-dump section breakdown for a session.
    Context { session: String },
    /// Aggregate provider cost snapshots across all agents.
    Costs,
    /// Read, write, or validate the Legion config file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List channels or show channel status.
    Channels {
        #[command(subcommand)]
        action: ChannelsAction,
    },
    /// Search memory or run local memory maintenance.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// List or validate locally configured skills.
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    /// Inspect configured MCP servers and their tools.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Run health checks.
    Doctor,
    /// Manage scheduled cron jobs.
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
    /// Schedule a recurring prompt (`/loop` non-interactive form).
    Loop {
        /// Cron expression or interval (e.g. "5m", "2h", "0 9 * * *").
        schedule: String,
        /// Prompt or slash command to run on each fire.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
        /// Run the prompt immediately after scheduling.
        #[arg(long, default_value = "true")]
        run_now: bool,
    },
    /// List or run declarative task flows (automation-advanced Phase C).
    Flows {
        #[command(subcommand)]
        action: FlowsAction,
    },
    /// List commitments inferred from conversation (automation-advanced Phase B).
    Commitments {
        #[command(subcommand)]
        action: CommitmentsAction,
    },
    /// View background task records.
    Tasks {
        #[command(subcommand)]
        action: TasksAction,
    },
    /// Manage connected companion nodes.
    Nodes {
        #[command(subcommand)]
        action: NodesAction,
    },
    /// Browse and manage plugins in the market.
    Market {
        #[command(subcommand)]
        action: MarketAction,
    },
    /// Run the first-time setup wizard.
    Setup {
        /// Skip interactive prompts and use provided values.
        #[arg(long)]
        non_interactive: bool,
        /// Provider preset: minimax, openai, anthropic, gemini, ollama,
        /// openrouter, bedrock, or custom.
        #[arg(long)]
        provider: Option<String>,
        /// API key for the selected provider (may be an env reference like
        /// `${OPENAI_API_KEY}`).
        #[arg(long)]
        api_key: Option<String>,
        /// Deprecated alias for `--api-key` (implies `--provider minimax`).
        #[arg(long, hide = true)]
        minimax_key: Option<String>,
        /// Default model override for the selected provider.
        #[arg(long)]
        model: Option<String>,
        /// Base URL override (required for `--provider custom`).
        #[arg(long)]
        base_url: Option<String>,
        /// Gateway authentication token.
        #[arg(long)]
        gateway_token: Option<String>,
        /// Gateway bind host.
        #[arg(long)]
        bind_host: Option<String>,
        /// Gateway port.
        #[arg(long)]
        port: Option<u16>,
        /// Overwrite an existing configuration (a .bak backup is written).
        #[arg(long)]
        force: bool,
        /// Merge the selected provider into an existing configuration instead
        /// of rewriting it.
        #[arg(long)]
        add_provider: bool,
        /// Install the gateway as a system service (launchd / systemd user
        /// unit / Windows logon task) after writing the configuration.
        #[arg(long)]
        install_daemon: bool,
    },
}

#[derive(Subcommand)]
enum GatewayAction {
    /// Start the Gateway (background by default).
    Start {
        /// Path to the config file.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Run in the foreground instead of daemonizing.
        #[arg(short, long)]
        foreground: bool,
        /// Allow downloading and installing a compatible gateway if none is installed.
        #[arg(long)]
        install: bool,
    },
    /// Stop the running background Gateway.
    Stop,
    /// Show gateway status.
    Status,
    /// Show recent gateway logs.
    Logs {
        /// Number of lines to show.
        #[arg(default_value = "100")]
        lines: usize,
    },
    /// Install a Gateway version from a manifest or local archive.
    Install {
        /// Version to install (latest compatible if omitted).
        #[arg(short, long)]
        version: Option<String>,
        /// Path to a local archive to install instead of downloading.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Target triple override.
        #[arg(long)]
        target: Option<String>,
        /// Release channel.
        #[arg(long, default_value = "stable")]
        channel: String,
    },
    /// List installed Gateway versions.
    ListVersions,
    /// Upgrade to a newer Gateway version.
    Upgrade {
        /// Target version (latest compatible if omitted).
        #[arg(long)]
        to: Option<String>,
        /// Restart the running gateway after upgrade.
        #[arg(long)]
        restart: bool,
    },
    /// Roll back to a previously installed Gateway version.
    Rollback {
        /// Target version (previous known-good if omitted).
        #[arg(long)]
        to: Option<String>,
        /// Restart the running gateway after rollback.
        #[arg(long)]
        restart: bool,
    },
    /// Remove old unreferenced Gateway versions.
    Prune {
        /// Number of extra versions to keep beyond current/previous/pinned.
        #[arg(long)]
        keep: Option<usize>,
    },
    /// Run gateway health and installation diagnostics.
    Doctor,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Read a config value by dotted key.
    Get { key: String },
    /// Set a config value by dotted key.
    Set { key: String, value: String },
    /// Validate the config file.
    Validate {
        /// Path to the config file.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ChannelsAction {
    /// List registered channels.
    List,
    /// Show channel status.
    Status,
}

#[derive(Subcommand)]
enum MemoryAction {
    /// Search memory via the Gateway.
    Search { query: String },
    /// Run decay + duplicate-merge maintenance locally (memory-layers Phase C).
    Merge,
}

#[derive(Subcommand)]
enum SkillsAction {
    /// List skills configured in `agents.defaults.skills.dirs`.
    List,
    /// Rescan skill directories and report load errors.
    Reload,
}

#[derive(Subcommand)]
enum McpAction {
    /// List configured MCP servers.
    List,
    /// Connect to configured servers and list their tools.
    Tools,
    /// Re-read config and attempt to connect to each server.
    Reload,
}

#[derive(Subcommand)]
enum CronAction {
    /// List scheduled cron jobs.
    List,
    /// Add a new cron job.
    Add {
        /// Cron expression (e.g. "0 9 * * *") or "__at__" for one-shot.
        /// Use "__webhook__" together with --webhook-secret for a job that
        /// only runs when a signed POST /webhook/<id> request arrives.
        schedule: String,
        /// Agent id to run the job as.
        #[arg(long)]
        agent: Option<String>,
        /// Message / instruction sent to the agent.
        #[arg(long)]
        message: Option<String>,
        /// One-shot run time (local ISO-8601 or "YYYY-MM-DD HH:MM:SS").
        #[arg(long)]
        at: Option<String>,
        /// HMAC-SHA256 secret enabling the /webhook/<id> trigger.
        #[arg(long)]
        webhook_secret: Option<String>,
    },
    /// Remove a cron job by id.
    Remove { id: String },
    /// Trigger a cron job manually by id.
    Run { id: String },
}

#[derive(Subcommand)]
enum FlowsAction {
    /// List task flows declared in the config.
    List,
    /// Run a task flow by id and print the step report.
    Run { id: String },
}

#[derive(Subcommand)]
enum CommitmentsAction {
    /// List inferred commitments (one-shot cron jobs with a `commitment:` id).
    List,
}

#[derive(Subcommand)]
enum TasksAction {
    /// List background task records.
    List,
    /// Show a single task record.
    Show { id: String },
    /// Enqueue a new background task.
    Create {
        #[arg(default_value = "main")]
        agent_id: String,
        message: Vec<String>,
        #[arg(long)]
        depends_on: Vec<String>,
    },
    /// Run a pending background task immediately.
    Run { id: String },
}

#[derive(Subcommand)]
enum NodesAction {
    /// List connected nodes.
    List,
    /// Show a single node status.
    Status { node_id: String },
    /// Invoke a command on a node.
    Invoke {
        node_id: String,
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Timeout in milliseconds.
        #[arg(long, default_value = "30000")]
        timeout_ms: u64,
    },
}

#[derive(Subcommand)]
enum MarketAction {
    /// List available plugins.
    List,
    /// Mark a plugin as installed.
    Install { id: String },
    /// Mark a plugin as uninstalled.
    Uninstall { id: String },
}

/// Run the local `memory merge` maintenance command: open the configured memory
/// backend, apply the configured decay/merge settings, and run one
/// `decay_and_merge` pass. Refuses to run when `memory.merge.enabled` is false so
/// an accidental invocation cannot drop data.
async fn run_memory_merge(config: &Config) -> Result<(), CliError> {
    if !config.memory.merge.enabled {
        return Err(CliError::Other(
            "memory.merge is disabled; set `memory.merge.enabled = true` (and optionally \
             `memory.merge.model`) in legion.json to enable duplicate merging."
                .to_string(),
        ));
    }
    let backend = open_memory_backend(config).await?;
    let report = backend
        .decay_and_merge()
        .await
        .map_err(|e| CliError::Other(format!("memory merge failed: {e}")))?;
    println!(
        "memory merge complete: merged={} dropped={}",
        report.merged, report.dropped
    );
    Ok(())
}

/// List commitments inferred from conversation. Reads the gateway's cron store
/// (`~/.legion/automation/cron.jsonl`) locally and prints the one-shot jobs
/// whose id carries the `commitment:` prefix.
async fn list_commitments() -> Result<(), CliError> {
    let path = dirs::home_dir()
        .map(|h| h.join(".legion").join("automation").join("cron.jsonl"))
        .unwrap_or_else(|| PathBuf::from(".legion/automation/cron.jsonl"));
    let store = legion_automation::cron::JsonlCronJobStore::open(&path)
        .await
        .map_err(|e| CliError::Other(format!("cron store: {e}")))?;
    use legion_automation::cron::CronJobStore;
    let mut jobs: Vec<_> = store
        .list()
        .await
        .map_err(|e| CliError::Other(format!("cron store: {e}")))?
        .into_iter()
        .filter(|j| j.id.starts_with("commitment:"))
        .collect();
    jobs.sort_by_key(|a| a.at);
    if jobs.is_empty() {
        println!("no inferred commitments");
        return Ok(());
    }
    for job in jobs {
        let due = job
            .at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_string());
        let state = if job.enabled { "enabled" } else { "disabled" };
        println!("{due}  [{state}]  {}", job.message);
    }
    Ok(())
}

async fn open_memory_backend(config: &Config) -> Result<SqliteVecBackend, CliError> {
    let collection_path = config
        .memory
        .builtin
        .collection_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".legion").join("memory")))
        .unwrap_or_else(|| PathBuf::from(".legion/memory"));
    let workspace = resolve_workspace(config, "main", None);
    let dimension = config.memory.builtin.embedding_dimension;

    let embedder: Arc<dyn Embedder> = match &config.memory.builtin.embedding_provider {
        Some(model_ref) => {
            let router = Arc::new(build_provider_router(config)?);
            Arc::new(ProviderEmbedder::new(router, model_ref, dimension))
        }
        None => Arc::new(FakeEmbedder::new(dimension)),
    };

    SqliteVecBackend::open(&collection_path, &workspace, embedder)
        .await
        .map(|b| {
            b.with_decay_config(config.memory.decay.clone())
                .with_merge_config(config.memory.merge.clone())
        })
        .map_err(|e| CliError::Other(format!("memory backend: {e}")))
}

/// Run one agent turn against an embedded runtime (no gateway process),
/// printing the same events the WebSocket path would.
async fn run_embedded_agent_turn(
    config: &Config,
    session_key: &str,
    text: String,
    dump_prompts: bool,
    yolo: bool,
    workspace_override: Option<PathBuf>,
) -> Result<(), CliError> {
    let host = legion_cli::driver::build_local_host(config).await?;
    legion_cli::driver::run_local_turn(
        &host,
        session_key,
        text,
        dump_prompts,
        yolo,
        workspace_override,
        |frame| {
            if let legion_protocol::WsFrame::Event { payload, .. } = frame {
                let _ = legion_cli::print_agent_event(&payload);
            }
        },
    )
    .await
}

/// Expand a leading `~` in a path string using `HOME`. Mirrors
/// `legion_runtime::expand_tilde` (which is `pub(crate)`).
fn expand_tilde_cli(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Resolve the `--workspace` flag into a per-run override.
///
/// - `None` (flag not passed) → current directory (the default, matching
///   mainstream coding agents: the agent works where the user launched it).
/// - `Some("none")` → `None`, i.e. fall back to the config default
///   (`~/.legion/workspace`, the legacy behavior).
/// - `Some(path)` → that path, with a leading `~` expanded.
pub(crate) fn resolve_workspace_override(cli_ws: &Option<String>) -> Option<PathBuf> {
    match cli_ws {
        None => Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        Some(value) if value == "none" => None,
        Some(path) => Some(expand_tilde_cli(path)),
    }
}

fn build_provider_router(config: &Config) -> Result<ProviderRouter, CliError> {
    let auth_profiles = load_auth_profiles("main").unwrap_or_default();
    let costs_path = dirs::home_dir().map(|h| h.join(".legion/agents/main/costs.json"));
    let mut router = ProviderRouter::from_configs(
        &config.models.providers,
        &auth_profiles,
        &config.models.costs,
        costs_path,
    )
    .map_err(|e| CliError::Other(format!("provider router: {e}")))?;
    router.set_aliases(config.models.aliases.clone());
    router.set_fallbacks(config.models.fallbacks.clone());
    Ok(router)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        None => {
            let workspace_override = resolve_workspace_override(&cli.workspace);
            match legion_cli::tui::run_tui(
                cli.session,
                legion_cli::driver::resolve_cli_mode(cli.local, cli.gateway),
                cli.yolo,
                workspace_override,
            )
            .await
            {
                Err(CliError::Cancelled) => {
                    eprintln!("Setup cancelled.");
                    std::process::exit(130);
                }
                result => result?,
            }
        }
        Some(Command::Gateway { action }) => match action {
            GatewayAction::Start {
                config,
                foreground,
                install,
            } => start_gateway_with_options(config, foreground, install).await?,
            GatewayAction::Stop => stop_gateway()?,
            GatewayAction::Status => {
                let config = load_config()?;
                let manager = GatewayManager::default_manager()?;
                println!("{}", manager.status(&config).await?)
            }
            GatewayAction::Logs { lines } => gateway_logs(lines)?,
            GatewayAction::Install {
                version,
                from,
                target,
                channel,
            } => {
                let manager = GatewayManager::default_manager()?;
                let path = if let Some(archive) = from {
                    let version = version.ok_or_else(|| {
                        CliError::Other(
                            "--version is required when installing from a local archive"
                                .to_string(),
                        )
                    })?;
                    manager.install_from_archive(&archive, &version, target.as_deref())?
                } else {
                    let config = load_config()?;
                    let url = default_manifest_url(&config).ok_or_else(|| {
                        CliError::Other(
                            "no manifest URL configured; set LEGION_RELEASES_URL or gateway.manifestUrl, or use --from"
                                .to_string(),
                        )
                    })?;
                    manager
                        .install_from_manifest(&url, version.as_deref(), &channel, false)
                        .await?
                };
                println!("installed legion-gateway at {}", path.display());
            }
            GatewayAction::ListVersions => {
                let manager = GatewayManager::default_manager()?;
                let versions = manager.list_versions()?;
                if versions.is_empty() {
                    println!("no installed gateway versions");
                } else {
                    for v in versions {
                        println!(
                            "{} {} ({}) installed at {} from {}",
                            v.version, v.target, v.release_id, v.installed_at, v.source
                        );
                    }
                }
            }
            GatewayAction::Upgrade { to, restart } => {
                let config = load_config()?;
                let manager = GatewayManager::default_manager()?;
                let url = default_manifest_url(&config);
                println!(
                    "{}",
                    manager
                        .upgrade(to.as_deref(), restart, url.as_deref(), None)
                        .await?
                );
            }
            GatewayAction::Rollback { to, restart } => {
                let manager = GatewayManager::default_manager()?;
                println!("{}", manager.rollback(to.as_deref(), restart).await?);
            }
            GatewayAction::Prune { keep } => {
                let manager = GatewayManager::default_manager()?;
                let removed = manager.prune(keep.unwrap_or(2))?;
                if removed.is_empty() {
                    println!("no versions pruned");
                } else {
                    for path in removed {
                        println!("removed {}", path.display());
                    }
                }
            }
            GatewayAction::Doctor => {
                let config = load_config()?;
                let manager = GatewayManager::default_manager()?;
                println!("{}", manager.doctor(&config).await?);
            }
        },
        Some(Command::Agent {
            dump_prompts,
            session,
            local,
            gateway,
            yolo,
            workspace,
            message,
        }) => {
            let text = message.join(" ");
            let session_key = match session {
                Some(value) => resolve_session_key_arg(&value, "cli")?,
                None => DEFAULT_CLI_SESSION_KEY.to_string(),
            };
            let config = load_config()?;
            let workspace_override = resolve_workspace_override(&workspace);
            if yolo {
                eprintln!("yolo mode: tool approvals are auto-accepted");
            }
            // Both embedded and gateway modes honor the cwd override; report
            // the resolved workspace so the user knows where the agent works.
            {
                let ws = workspace_override
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "config default".to_string());
                eprintln!("workspace: {ws}");
            }
            match legion_cli::driver::resolve_cli_mode(local, gateway) {
                CliMode::Gateway => {
                    let client = GatewayClient::connect(&config).await?;
                    if let Some(warning) = client.version_warning() {
                        eprintln!("warning: {warning}");
                    }
                    client
                        .agent_turn(
                            &text,
                            dump_prompts,
                            yolo,
                            &session_key,
                            workspace_override.as_deref(),
                        )
                        .await?;
                    client.close().await;
                }
                CliMode::Auto => match legion_cli::driver::probe_gateway(&config).await {
                    Some(client) => {
                        if let Some(warning) = client.version_warning() {
                            eprintln!("warning: {warning}");
                        }
                        client
                            .agent_turn(
                                &text,
                                dump_prompts,
                                yolo,
                                &session_key,
                                workspace_override.as_deref(),
                            )
                            .await?;
                        client.close().await;
                    }
                    None => {
                        eprintln!("{}", legion_cli::driver::EMBEDDED_NOTICE);
                        run_embedded_agent_turn(
                            &config,
                            &session_key,
                            text,
                            dump_prompts,
                            yolo,
                            workspace_override,
                        )
                        .await?;
                    }
                },
                CliMode::Local => {
                    eprintln!("{}", legion_cli::driver::EMBEDDED_NOTICE);
                    run_embedded_agent_turn(
                        &config,
                        &session_key,
                        text,
                        dump_prompts,
                        yolo,
                        workspace_override,
                    )
                    .await?;
                }
            }
        }
        Some(Command::Context { session }) => {
            show_context(&session)?;
        }
        Some(Command::Costs) => {
            legion_cli::costs::run()?;
        }
        Some(Command::Config { action }) => match action {
            ConfigAction::Get { key } => {
                let path = default_config_path().ok_or_else(|| {
                    CliError::Other("unable to determine config path".to_string())
                })?;
                match config_get(&path, &key)? {
                    Some(value) => println!("{}", serde_json::to_string_pretty(&value)?),
                    None => println!("null"),
                }
            }
            ConfigAction::Set { key, value } => {
                let path = default_config_path().ok_or_else(|| {
                    CliError::Other("unable to determine config path".to_string())
                })?;
                config_set(&path, &key, &value)?;
                println!("ok");
            }
            ConfigAction::Validate { config } => {
                let path = config.or_else(default_config_path).ok_or_else(|| {
                    CliError::Other("unable to determine config path".to_string())
                })?;
                validate_config(&path)?;
                println!("config is valid");
            }
        },
        Some(Command::Channels { action }) => {
            let config = load_config()?;
            let client = GatewayClient::connect(&config).await?;
            match action {
                ChannelsAction::List => {
                    let resp = client.request("channels", json!({})).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                ChannelsAction::Status => {
                    let resp = client.request("channels", json!({})).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
            }
            client.close().await;
        }
        Some(Command::Memory { action }) => {
            let config = load_config()?;
            match action {
                MemoryAction::Search { query } => {
                    let client = GatewayClient::connect(&config).await?;
                    let resp = client
                        .request("memory.search", json!({ "query": query, "top_k": 5 }))
                        .await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                    client.close().await;
                }
                MemoryAction::Merge => {
                    run_memory_merge(&config).await?;
                }
            }
        }
        Some(Command::Skills { action }) => {
            let config = load_config()?;
            match action {
                SkillsAction::List => legion_cli::skills::list(&config).await?,
                SkillsAction::Reload => legion_cli::skills::reload(&config).await?,
            }
        }
        Some(Command::Mcp { action }) => {
            let config = load_config()?;
            match action {
                McpAction::List => legion_cli::mcp::list(&config).await?,
                McpAction::Tools => legion_cli::mcp::tools(&config).await?,
                McpAction::Reload => legion_cli::mcp::reload(&config).await?,
            }
        }
        Some(Command::Doctor) => doctor().await?,
        Some(Command::Cron { action }) => {
            let config = load_config()?;
            let client = GatewayClient::connect(&config).await?;
            match action {
                CronAction::List => {
                    let resp = client.request("cron.list", json!({})).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                CronAction::Add {
                    schedule,
                    agent,
                    message,
                    at,
                    webhook_secret,
                } => {
                    let params = json!({
                        "schedule": schedule,
                        "agent_id": agent.unwrap_or_else(|| "main".to_string()),
                        "message": message.unwrap_or_default(),
                        "at": at,
                        "webhook_secret": webhook_secret,
                    });
                    let resp = client.request("cron.add", params).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                CronAction::Remove { id } => {
                    let resp = client.request("cron.remove", json!({ "id": id })).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                CronAction::Run { id } => {
                    let resp = client.request("cron.run", json!({ "id": id })).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
            }
            client.close().await;
        }
        Some(Command::Loop {
            schedule,
            message,
            run_now,
        }) => {
            let text = message.join(" ");
            if text.trim().is_empty() {
                return Err(CliError::Other(
                    "missing prompt; usage: legion loop <interval> <prompt>".to_string(),
                )
                .into());
            }
            let config = load_config()?;
            let client = GatewayClient::connect(&config).await?;
            // Support both shorthand intervals ("5m") and full cron expressions.
            let input = format!("{schedule} {text}");
            let cron = match legion_cli::loop_cmd::parse_loop(&input) {
                Ok(req) => legion_cli::loop_cmd::interval_to_cron(&req.interval)
                    .map_err(|e| CliError::Other(e.to_string()))?,
                Err(_) => schedule.clone(),
            };
            let resp = client
                .request(
                    "cron.add",
                    json!({
                        "schedule": cron,
                        "agent_id": "main",
                        "message": text,
                    }),
                )
                .await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
            );
            if run_now {
                client
                    .agent_turn(
                        &text,
                        false,
                        false,
                        legion_cli::DEFAULT_CLI_SESSION_KEY,
                        None,
                    )
                    .await?;
            }
            client.close().await;
        }
        Some(Command::Commitments { action }) => match action {
            CommitmentsAction::List => {
                list_commitments().await?;
            }
        },
        Some(Command::Flows { action }) => {
            let config = load_config()?;
            let client = GatewayClient::connect(&config).await?;
            match action {
                FlowsAction::List => {
                    let resp = client.request("flows.list", json!({})).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                FlowsAction::Run { id } => {
                    let resp = client.request("flows.run", json!({ "id": id })).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
            }
            client.close().await;
        }
        Some(Command::Tasks { action }) => {
            let config = load_config()?;
            let client = GatewayClient::connect(&config).await?;
            match action {
                TasksAction::List => {
                    let resp = client.request("tasks.list", json!({})).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                TasksAction::Show { id } => {
                    let resp = client.request("tasks.show", json!({ "id": id })).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                TasksAction::Create {
                    agent_id,
                    message,
                    depends_on,
                } => {
                    let text = message.join(" ");
                    let resp = client
                        .request(
                            "tasks.create",
                            json!({
                                "agent_id": agent_id,
                                "message": text,
                                "depends_on": depends_on,
                            }),
                        )
                        .await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                TasksAction::Run { id } => {
                    let resp = client.request("tasks.run", json!({ "id": id })).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
            }
            client.close().await;
        }
        Some(Command::Nodes { action }) => {
            let config = load_config()?;
            let client = GatewayClient::connect(&config).await?;
            match action {
                NodesAction::List => {
                    let resp = client.request("nodes.list", json!({})).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                NodesAction::Status { node_id } => {
                    let resp = client
                        .request("nodes.status", json!({ "node_id": node_id }))
                        .await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                NodesAction::Invoke {
                    node_id,
                    command,
                    args,
                    timeout_ms,
                } => {
                    let params = if args.is_empty() {
                        json!({})
                    } else {
                        json!({ "args": args })
                    };
                    let resp = client
                        .request(
                            "node.invoke",
                            json!({
                                "node_id": node_id,
                                "command": command,
                                "params": params,
                                "timeout_ms": timeout_ms,
                            }),
                        )
                        .await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
            }
            client.close().await;
        }
        Some(Command::Market { action }) => {
            let config = load_config()?;
            let client = GatewayClient::connect(&config).await?;
            match action {
                MarketAction::List => {
                    let resp = client.request("market.list", json!({})).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                MarketAction::Install { id } => {
                    let resp = client
                        .request("market.install", json!({ "id": id }))
                        .await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
                MarketAction::Uninstall { id } => {
                    let resp = client
                        .request("market.uninstall", json!({ "id": id }))
                        .await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp.get("payload").unwrap_or(&json!(null)))?
                    );
                }
            }
            client.close().await;
        }
        Some(Command::Setup {
            non_interactive,
            provider,
            api_key,
            minimax_key,
            model,
            base_url,
            gateway_token,
            bind_host,
            port,
            force,
            add_provider,
            install_daemon,
        }) => {
            let home = dirs::home_dir()
                .ok_or_else(|| CliError::Other("unable to determine home directory".to_string()))?;
            // `--minimax-key` is a deprecated alias for `--api-key` that
            // implies the minimax provider.
            let provider = provider.or_else(|| minimax_key.as_ref().map(|_| "minimax".to_string()));
            let opts = SetupOptions {
                provider,
                api_key: api_key.or(minimax_key),
                model,
                base_url,
                gateway_token,
                bind_host,
                port,
                force,
                add_provider,
                install_daemon,
            };
            match legion_cli::setup::run_setup(!non_interactive, opts, &home).await {
                Err(CliError::Cancelled) => {
                    eprintln!("Setup cancelled.");
                    std::process::exit(130);
                }
                result => result?,
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_override_defaults_to_current_dir() {
        let got = resolve_workspace_override(&None);
        assert_eq!(got, Some(std::env::current_dir().unwrap()));
    }

    #[test]
    fn workspace_override_none_keyword_falls_back_to_config() {
        assert_eq!(resolve_workspace_override(&Some("none".to_string())), None);
    }

    #[test]
    fn workspace_override_explicit_path_is_kept() {
        assert_eq!(
            resolve_workspace_override(&Some("/tmp/project".to_string())),
            Some(PathBuf::from("/tmp/project"))
        );
    }

    #[test]
    fn workspace_override_tilde_is_expanded() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        assert_eq!(
            resolve_workspace_override(&Some("~/projects".to_string())),
            Some(PathBuf::from(&home).join("projects"))
        );
    }
}
