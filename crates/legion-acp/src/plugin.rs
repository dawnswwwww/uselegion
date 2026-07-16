use async_trait::async_trait;
use legion_plugin_sdk::{
    Plugin, PluginContext, PluginError, PluginHandles, PluginKind, PluginMetadata,
};

/// System plugin that registers the ACP external harness bridge.
#[derive(Debug)]
pub struct AcpPlugin;

impl AcpPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AcpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for AcpPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:acp-bridge".to_string(),
            name: "ACP Bridge".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Harness,
            description: Some("Agent Connect Protocol external harness bridge".to_string()),
        }
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("ACP bridge plugin initialized");
        Ok(PluginHandles::default())
    }
}

/// Convenience constructor for the plugin as a boxed, dynamically-dispatched plugin.
pub fn boxed_plugin() -> legion_plugin_sdk::BoxedPlugin {
    Box::new(AcpPlugin::new())
}
