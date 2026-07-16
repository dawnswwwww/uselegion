use thiserror::Error;

/// Errors that can occur while assembling or running an [`AgentHost`](crate::host::AgentHost).
#[derive(Debug, Error)]
pub enum HostError {
    #[error("invalid configuration: {0}")]
    Config(#[from] legion_core::config::ConfigError),

    #[error("plugin error: {0}")]
    Plugin(#[from] legion_plugin_sdk::PluginError),

    #[error("channel error: {0}")]
    Channel(#[from] legion_plugin_sdk::channel::ChannelError),

    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("automation error: {0}")]
    Automation(String),
}
