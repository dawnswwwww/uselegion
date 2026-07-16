use async_trait::async_trait;
use legion_acp::AcpPlugin;
use legion_channel::{
    DiscordProvider, LarkProvider, MatrixProvider, SlackProvider, TelegramProvider, WebChatProvider,
};
use legion_plugin_sdk::{
    Capability, Plugin, PluginContext, PluginError, PluginHandles, PluginKind, PluginMetadata,
    PluginRegistry,
};
use legion_tools::ToolsPlugin;
use std::sync::Arc;

/// All system plugins loaded by the host, including the built-in channel
/// providers that need to be reachable from the Gateway distribution layer.
pub struct SystemPlugins {
    pub registry: PluginRegistry,
    pub webchat: Arc<WebChatProvider>,
    pub telegram: Arc<TelegramProvider>,
    pub slack: Arc<SlackProvider>,
    pub discord: Arc<DiscordProvider>,
    pub lark: Arc<LarkProvider>,
    pub matrix: Arc<MatrixProvider>,
}

/// Load all hard-coded system plugins for the MVP host.
///
/// The returned providers are *constructed* but not *started*; lifecycle
/// belongs to the Gateway distribution layer.
pub async fn load_system_plugins() -> Result<SystemPlugins, PluginError> {
    let mut registry = PluginRegistry::new();

    // Core tools plugin.
    registry.load(Box::new(ToolsPlugin::new()))?;

    // Built-in channel providers. We keep concrete Arcs for the Gateway while
    // also exposing them through the plugin registry as capabilities.
    let webchat = Arc::new(WebChatProvider::new());
    let telegram = Arc::new(TelegramProvider::new());
    let slack = Arc::new(SlackProvider::new());
    let discord = Arc::new(DiscordProvider::new());
    let lark = Arc::new(LarkProvider::new());
    let matrix = Arc::new(MatrixProvider::new());
    registry.load(Box::new(WebChatPlugin::new(webchat.clone())))?;
    registry.load(Box::new(TelegramPlugin::new(telegram.clone())))?;
    registry.load(Box::new(SlackPlugin::new(slack.clone())))?;
    registry.load(Box::new(DiscordPlugin::new(discord.clone())))?;
    registry.load(Box::new(LarkPlugin::new(lark.clone())))?;
    registry.load(Box::new(MatrixPlugin::new(matrix.clone())))?;

    // Stub system plugins that mirror the PRD system plugin list.
    // These declare no capabilities until the real crates expose Plugin impls.
    registry.load(Box::new(MemoryPlugin))?;
    registry.load(Box::new(ProviderRouterPlugin))?;
    registry.load(Box::new(ContextEnginePlugin))?;
    registry.load(Box::new(AutomationPlugin))?;
    registry.load(Box::new(AcpPlugin::new()))?;

    Ok(SystemPlugins {
        registry,
        webchat,
        telegram,
        slack,
        discord,
        lark,
        matrix,
    })
}

macro_rules! stub_plugin {
    ($name:ident, $id:expr, $plugin_kind:expr, $desc:expr) => {
        #[derive(Debug)]
        pub struct $name;

        #[async_trait]
        impl Plugin for $name {
            fn metadata(&self) -> PluginMetadata {
                PluginMetadata {
                    id: $id.to_string(),
                    name: stringify!($name).to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    kind: $plugin_kind,
                    description: Some($desc.to_string()),
                }
            }

            fn capabilities(&self) -> Vec<Capability> {
                vec![]
            }

            async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
                tracing::info!(plugin = $id, "system plugin initialized");
                Ok(PluginHandles::default())
            }
        }
    };
}

/// System plugin wrapper for the WebChat channel provider.
#[derive(Debug, Clone)]
pub struct WebChatPlugin {
    provider: Arc<WebChatProvider>,
}

impl WebChatPlugin {
    pub fn new(provider: Arc<WebChatProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Plugin for WebChatPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:channel-webchat".to_string(),
            name: "WebChat Channel".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Channel,
            description: Some("Built-in WebSocket/Web UI channel provider".to_string()),
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Channel]
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("webchat channel plugin initialized");
        Ok(PluginHandles {
            channels: vec![self.provider.clone()],
            skills: Vec::new(),
        })
    }
}

/// System plugin wrapper for the Telegram channel provider.
#[derive(Debug, Clone)]
pub struct TelegramPlugin {
    provider: Arc<TelegramProvider>,
}

impl TelegramPlugin {
    pub fn new(provider: Arc<TelegramProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Plugin for TelegramPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:channel-telegram".to_string(),
            name: "Telegram Channel".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Channel,
            description: Some("Built-in Telegram Bot channel provider".to_string()),
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Channel]
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("telegram channel plugin initialized");
        Ok(PluginHandles {
            channels: vec![self.provider.clone()],
            skills: Vec::new(),
        })
    }
}

/// System plugin wrapper for the Slack channel provider.
#[derive(Debug, Clone)]
pub struct SlackPlugin {
    provider: Arc<SlackProvider>,
}

impl SlackPlugin {
    pub fn new(provider: Arc<SlackProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Plugin for SlackPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:channel-slack".to_string(),
            name: "Slack Channel".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Channel,
            description: Some("Built-in Slack (Socket Mode) channel provider".to_string()),
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Channel]
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("slack channel plugin initialized");
        Ok(PluginHandles {
            channels: vec![self.provider.clone()],
            skills: Vec::new(),
        })
    }
}

/// System plugin wrapper for the Discord channel provider.
#[derive(Debug, Clone)]
pub struct DiscordPlugin {
    provider: Arc<DiscordProvider>,
}

impl DiscordPlugin {
    pub fn new(provider: Arc<DiscordProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Plugin for DiscordPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:channel-discord".to_string(),
            name: "Discord Channel".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Channel,
            description: Some("Built-in Discord (Gateway) channel provider".to_string()),
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Channel]
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("discord channel plugin initialized");
        Ok(PluginHandles {
            channels: vec![self.provider.clone()],
            skills: Vec::new(),
        })
    }
}

/// System plugin wrapper for the Lark channel provider.
#[derive(Debug, Clone)]
pub struct LarkPlugin {
    provider: Arc<LarkProvider>,
}

impl LarkPlugin {
    pub fn new(provider: Arc<LarkProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Plugin for LarkPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:channel-lark".to_string(),
            name: "Lark Channel".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Channel,
            description: Some(
                "Built-in Lark/Feishu (long-connection) channel provider".to_string(),
            ),
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Channel]
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("lark channel plugin initialized");
        Ok(PluginHandles {
            channels: vec![self.provider.clone()],
            skills: Vec::new(),
        })
    }
}

/// System plugin wrapper for the Matrix channel provider.
#[derive(Debug, Clone)]
pub struct MatrixPlugin {
    provider: Arc<MatrixProvider>,
}

impl MatrixPlugin {
    pub fn new(provider: Arc<MatrixProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Plugin for MatrixPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:channel-matrix".to_string(),
            name: "Matrix Channel".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Channel,
            description: Some("Built-in Matrix (client-server sync) channel provider".to_string()),
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Channel]
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("matrix channel plugin initialized");
        Ok(PluginHandles {
            channels: vec![self.provider.clone()],
            skills: Vec::new(),
        })
    }
}

stub_plugin!(
    MemoryPlugin,
    "system:memory-sqlite-zvec",
    PluginKind::Memory,
    "Memory backend plugin (SQLite + ZVec)"
);

stub_plugin!(
    ProviderRouterPlugin,
    "system:provider-router",
    PluginKind::Harness,
    "Multi-model LLM provider router"
);

stub_plugin!(
    ContextEnginePlugin,
    "system:context-legacy",
    PluginKind::ContextEngine,
    "Default legacy context engine"
);

stub_plugin!(
    AutomationPlugin,
    "system:automation-cron",
    PluginKind::Diagnostics,
    "Cron + heartbeat automation plugin"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_system_plugins_registers_expected_ids() {
        let plugins = load_system_plugins().await.unwrap();
        let ids: Vec<String> = plugins
            .registry
            .list()
            .iter()
            .map(|p| p.metadata().id.clone())
            .collect();

        let expected = [
            "system:tools-core",
            "system:channel-webchat",
            "system:channel-telegram",
            "system:channel-slack",
            "system:channel-discord",
            "system:channel-lark",
            "system:channel-matrix",
            "system:memory-sqlite-zvec",
            "system:provider-router",
            "system:context-legacy",
            "system:automation-cron",
            "system:acp-bridge",
        ];
        assert_eq!(ids.len(), expected.len());
        for id in expected {
            assert!(
                ids.iter().any(|registered| registered == id),
                "missing system plugin: {id}"
            );
        }
    }
}
