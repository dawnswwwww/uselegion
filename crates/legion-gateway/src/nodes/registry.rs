//! In-memory registry of connected and paired nodes.

use legion_core::util::lock_recover;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A connected node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub display_name: String,
    pub platform: String,
    pub device_family: String,
    pub commands: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<HashMap<String, bool>>,
    #[serde(skip)]
    pub paired: bool,
}

impl Node {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        platform: impl Into<String>,
        device_family: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            platform: platform.into(),
            device_family: device_family.into(),
            commands: Vec::new(),
            capabilities: Vec::new(),
            permissions: None,
            paired: false,
        }
    }

    pub fn with_commands(mut self, commands: Vec<String>) -> Self {
        self.commands = commands;
        self
    }

    pub fn with_capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_permissions(mut self, permissions: HashMap<String, bool>) -> Self {
        self.permissions = Some(permissions);
        self
    }
}

/// Registry of known/connected nodes.
#[derive(Default, Clone)]
pub struct NodeRegistry {
    inner: Arc<Mutex<NodeRegistryState>>,
}

#[derive(Default)]
struct NodeRegistryState {
    nodes: HashMap<String, Node>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, node: Node) {
        let mut state = lock_recover(&self.inner);
        state.nodes.insert(node.id.clone(), node);
    }

    pub fn unregister(&self, id: &str) {
        let mut state = lock_recover(&self.inner);
        state.nodes.remove(id);
    }

    pub fn get(&self, id: &str) -> Option<Node> {
        let state = lock_recover(&self.inner);
        state.nodes.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Node> {
        let state = lock_recover(&self.inner);
        state.nodes.values().cloned().collect()
    }
}
