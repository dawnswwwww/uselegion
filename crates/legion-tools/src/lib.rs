pub mod ask_user;
pub mod background_task;
pub mod browser;
pub mod grep;
pub mod list_dir;
pub mod mcp;
pub mod policy;
pub mod registry;
pub mod sandbox;
pub mod todo;
pub mod tools;

use async_trait::async_trait;
use legion_plugin_sdk::{
    Plugin, PluginContext, PluginError, PluginHandles, PluginKind, PluginMetadata,
};
use std::sync::Arc;

pub use policy::{Approval, Policy};
pub use registry::CoreToolRegistry;

/// Plugin entry point that registers the core tools system with the Gateway.
#[derive(Debug)]
pub struct ToolsPlugin;

impl ToolsPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ToolsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for ToolsPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "system:tools-core".to_string(),
            name: "Legion Core Tools".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: PluginKind::Tool,
            description: Some(
                "Built-in tools: read, list_dir, grep, write, edit, apply_patch, exec, web_search, web_fetch, ask_user"
                    .to_string(),
            ),
        }
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        tracing::info!("legion-tools core plugin initialized");
        Ok(PluginHandles::default())
    }
}

/// Convenience constructor for the plugin as a boxed, dynamically-dispatched plugin.
pub fn boxed_plugin() -> legion_plugin_sdk::BoxedPlugin {
    Box::new(ToolsPlugin::new())
}

/// Convenience constructor for the core tool registry.
pub fn core_registry(config: &legion_core::config::Config) -> Arc<CoreToolRegistry> {
    Arc::new(CoreToolRegistry::new(config))
}
