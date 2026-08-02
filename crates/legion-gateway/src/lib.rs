pub mod error;
pub mod events;
pub mod gateway;
pub mod http;
pub mod market;
pub mod message;
pub mod nodes;
pub mod observability;
pub mod pairing;
pub mod websocket;
mod ws_rpc;

// Re-export transport-neutral host types from legion-host so existing callers
// (including integration tests and the CLI during the Phase 2 transition)
// keep compiling. These will be removed in Phase 3 once all consumers import
// from `legion_host` directly.
pub use legion_host::{
    AgentHost, SessionStore, drive_run_stream, host, metrics, recover_orphaned_tool_results,
    routing, session, session_tools, system_plugins, turn,
};

pub use error::GatewayError;
pub use gateway::Gateway;
pub use message::WsFrame;
pub use pairing::{Device, PairingStore};

use legion_core::config::Config;
use std::path::PathBuf;
use tracing::{error, info};

/// Load a config file, falling back to a permissive default if the file is missing.
pub fn load_config_file(path: &PathBuf) -> Result<Config, GatewayError> {
    if path.exists() {
        let text = std::fs::read_to_string(path).map_err(|e| {
            GatewayError::Config(legion_core::config::ConfigError::ParseError(e.to_string()))
        })?;
        if path.extension().is_some_and(|ext| ext == "json5") {
            Config::from_json5(&text).map_err(GatewayError::Config)
        } else {
            Config::from_json(&text).map_err(GatewayError::Config)
        }
    } else {
        let json = r#"{ "gateway": { "auth": { "mode": "none" } } }"#;
        Config::from_json(json).map_err(GatewayError::Config)
    }
}

/// Create and run a Gateway from a config file path.
///
/// This is the shared entry point used by both the `legion-gateway` standalone
/// binary and the `legion gateway start` CLI command.
pub async fn run_gateway(config_path: Option<PathBuf>) -> Result<(), GatewayError> {
    // Initialize tracing subscriber if it hasn't been set already (e.g. when
    // invoked from the `legion` CLI, which sets up its own subscriber).
    let _ = tracing_subscriber::fmt::try_init();

    let config_path = config_path
        .or_else(|| dirs::home_dir().map(|h| h.join(".legion").join("legion.json")))
        .expect("unable to determine config path");

    info!(path = %config_path.display(), "loading Legion config");

    let config = match load_config_file(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            error!(error = %err, "failed to load config");
            std::process::exit(1);
        }
    };

    let gateway = match Gateway::new(config).await {
        Ok(gw) => gw,
        Err(err) => {
            error!(error = %err, "failed to create gateway");
            std::process::exit(1);
        }
    };

    if let Err(err) = gateway.start().await {
        error!(error = %err, "gateway exited with error");
        std::process::exit(1);
    }

    Ok(())
}
