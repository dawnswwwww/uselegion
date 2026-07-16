//! Plugin market: catalog of available plugins and install tracking.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A plugin entry in the market catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarketPlugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub installed: bool,
}

impl MarketPlugin {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        version: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
            kind: kind.into(),
            description: None,
            installed: false,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// In-memory plugin market.
#[derive(Default, Clone)]
pub struct PluginMarket {
    inner: Arc<Mutex<PluginMarketState>>,
}

#[derive(Default)]
struct PluginMarketState {
    catalog: HashMap<String, MarketPlugin>,
}

impl PluginMarket {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the market with built-in/system plugins.
    pub fn with_system_plugins(self) -> Self {
        let system = vec![
            MarketPlugin::new("system:tools", "Core Tools", "0.1.0", "tools")
                .with_description("Core file, shell, and web tools"),
            MarketPlugin::new(
                "system:memory-sqlite-zvec",
                "SQLite + ZVec Memory",
                "0.1.0",
                "memory",
            )
            .with_description("Memory backend using SQLite and ZVec embeddings"),
            MarketPlugin::new(
                "system:provider-router",
                "Provider Router",
                "0.1.0",
                "harness",
            )
            .with_description("Multi-model LLM provider router"),
            MarketPlugin::new(
                "system:automation-cron",
                "Automation",
                "0.1.0",
                "diagnostics",
            )
            .with_description("Cron, heartbeat, and background tasks"),
        ];
        self.seed(system);
        self
    }

    /// Add plugins to the catalog.
    pub fn seed(&self, plugins: Vec<MarketPlugin>) {
        let mut state = self.inner.lock().unwrap();
        for plugin in plugins {
            state.catalog.insert(plugin.id.clone(), plugin);
        }
    }

    /// List all plugins in the catalog.
    pub fn list(&self) -> Vec<MarketPlugin> {
        let state = self.inner.lock().unwrap();
        state.catalog.values().cloned().collect()
    }

    /// Mark a plugin as installed. Returns false if the plugin is unknown.
    pub fn install(&self, id: &str) -> bool {
        let mut state = self.inner.lock().unwrap();
        match state.catalog.get_mut(id) {
            Some(plugin) => {
                plugin.installed = true;
                true
            }
            None => false,
        }
    }

    /// Mark a plugin as uninstalled. Returns false if the plugin is unknown.
    pub fn uninstall(&self, id: &str) -> bool {
        let mut state = self.inner.lock().unwrap();
        match state.catalog.get_mut(id) {
            Some(plugin) => {
                plugin.installed = false;
                true
            }
            None => false,
        }
    }

    /// Get a single plugin by id.
    pub fn get(&self, id: &str) -> Option<MarketPlugin> {
        let state = self.inner.lock().unwrap();
        state.catalog.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_populates_catalog() {
        let market = PluginMarket::new().with_system_plugins();
        let plugins = market.list();
        assert!(!plugins.is_empty());
        assert!(plugins.iter().any(|p| p.id == "system:tools"));
    }

    #[test]
    fn install_and_uninstall_toggle_state() {
        let market = PluginMarket::new().with_system_plugins();
        assert!(market.install("system:tools"));
        let plugin = market.get("system:tools").unwrap();
        assert!(plugin.installed);

        assert!(market.uninstall("system:tools"));
        let plugin = market.get("system:tools").unwrap();
        assert!(!plugin.installed);
    }

    #[test]
    fn install_unknown_plugin_returns_false() {
        let market = PluginMarket::new();
        assert!(!market.install("unknown"));
    }
}
