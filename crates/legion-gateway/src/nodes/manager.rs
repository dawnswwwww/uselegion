//! Node manager: registry, command routing, and synchronous invoke handling.

use super::registry::{Node, NodeRegistry};
use legion_core::util::lock_recover;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// Frame sent to a node connection.
#[derive(Debug, Clone)]
pub struct NodeFrame {
    pub node_id: String,
    pub payload: serde_json::Value,
}

type PendingInvocations = HashMap<String, (String, oneshot::Sender<serde_json::Value>)>;

/// Manager that owns the node registry and routes invocations to node connections.
#[derive(Clone)]
pub struct NodeManager {
    registry: NodeRegistry,
    /// Map from node id to a channel sender that forwards frames to the node's WS task.
    senders: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<serde_json::Value>>>>,
    /// Map from correlation id to the node id and a oneshot sender waiting for the
    /// node's response.
    pending: Arc<Mutex<PendingInvocations>>,
}

impl Default for NodeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeManager {
    pub fn new() -> Self {
        Self {
            registry: NodeRegistry::new(),
            senders: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn registry(&self) -> &NodeRegistry {
        &self.registry
    }

    /// Register a connected node and return a receiver for frames destined to it.
    pub fn connect(&self, node: Node) -> mpsc::UnboundedReceiver<serde_json::Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut senders = lock_recover(&self.senders);
        senders.insert(node.id.clone(), tx);
        self.registry.register(node);
        rx
    }

    /// Disconnect a node and clean up its sender.
    pub fn disconnect(&self, node_id: &str) {
        let mut senders = lock_recover(&self.senders);
        senders.remove(node_id);
        self.registry.unregister(node_id);
        // Drop pending invocations for this node without a response so that
        // waiting callers receive NodeDisconnected.
        let mut pending = lock_recover(&self.pending);
        pending.retain(|_, (nid, _)| nid != node_id);
    }

    /// Invoke a command on a connected node and wait for a response.
    pub async fn invoke(
        &self,
        node_id: &str,
        command: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, NodeInvokeError> {
        let sender = {
            let senders = lock_recover(&self.senders);
            senders
                .get(node_id)
                .cloned()
                .ok_or_else(|| NodeInvokeError::NodeNotFound(node_id.to_string()))?
        };

        let correlation = format!(
            "invoke-{}-{:x}",
            node_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = lock_recover(&self.pending);
            pending.insert(correlation.clone(), (node_id.to_string(), tx));
        }

        let frame = json!({
            "type": "node.invoke",
            "correlation": correlation,
            "command": command,
            "params": params,
        });

        debug!(node_id, command, %correlation, "sending node.invoke");
        sender
            .send(frame)
            .map_err(|_| NodeInvokeError::NodeDisconnected)?;

        let result = tokio::time::timeout(timeout, rx).await;
        {
            let mut pending = lock_recover(&self.pending);
            pending.remove(&correlation);
        }

        match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(NodeInvokeError::NodeDisconnected),
            Err(_) => Err(NodeInvokeError::Timeout),
        }
    }

    /// Complete a pending invocation with the node's response.
    pub fn resolve(&self, correlation: &str, response: serde_json::Value) {
        let tx = {
            let mut pending = lock_recover(&self.pending);
            pending.remove(correlation).map(|(_, tx)| tx)
        };
        if let Some(tx) = tx {
            let _ = tx.send(response);
        } else {
            warn!(%correlation, "no pending node invocation for correlation");
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NodeInvokeError {
    #[error("node '{0}' not found")]
    NodeNotFound(String),
    #[error("node disconnected")]
    NodeDisconnected,
    #[error("node invoke timed out")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::registry::Node;

    #[tokio::test]
    async fn invoke_returns_node_response() {
        let manager = NodeManager::new();
        let node = Node::new("n1", "Node One", "ios", "phone");
        let mut rx = manager.connect(node);

        let answer_manager = manager.clone();
        let answer = tokio::spawn(async move {
            if let Some(payload) = rx.recv().await {
                let correlation = payload["correlation"].as_str().unwrap().to_string();
                answer_manager.resolve(&correlation, json!({ "ok": true }));
            }
        });

        let response = manager
            .invoke("n1", "camera.list", json!({}), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(response, json!({ "ok": true }));
        answer.await.unwrap();
    }

    #[tokio::test]
    async fn invoke_fails_when_node_missing() {
        let manager = NodeManager::new();
        let err = manager
            .invoke("missing", "camera.list", json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, NodeInvokeError::NodeNotFound(_)));
    }

    #[tokio::test]
    async fn invoke_times_out_when_node_silent() {
        let manager = NodeManager::new();
        let node = Node::new("n2", "Node Two", "ios", "phone");
        let _rx = manager.connect(node);

        let err = manager
            .invoke("n2", "camera.list", json!({}), Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(matches!(err, NodeInvokeError::Timeout));
    }

    #[tokio::test]
    async fn disconnect_fails_pending_invocations() {
        let manager = NodeManager::new();
        let node = Node::new("n3", "Node Three", "ios", "phone");
        let mut rx = manager.connect(node);

        let invoke_manager = manager.clone();
        let invoke = tokio::spawn(async move {
            invoke_manager
                .invoke("n3", "camera.list", json!({}), Duration::from_secs(5))
                .await
        });

        // Wait until the invoke frame is delivered to the node; at that point
        // the correlation is registered in pending.
        let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame["type"], "node.invoke");

        manager.disconnect("n3");

        let err = invoke.await.unwrap().unwrap_err();
        assert!(matches!(err, NodeInvokeError::NodeDisconnected));
    }
}
