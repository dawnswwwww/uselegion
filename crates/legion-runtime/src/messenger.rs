use async_trait::async_trait;
use thiserror::Error;

/// Errors returned by an [`AgentMessenger`] delivery attempt.
#[derive(Debug, Error)]
pub enum MessengerError {
    /// The target agent is not present in `agents.list`.
    #[error("unknown agent '{0}'")]
    UnknownAgent(String),
    /// The target agent's `allowFrom` does not include the sender.
    #[error("agent '{from}' is not allowed to send to agent '{to}'")]
    NotAllowed { from: String, to: String },
    /// Delivery failed after the policy check passed.
    #[error("agent message delivery failed: {0}")]
    Runtime(String),
}

/// Fire-and-forget delivery of a message from one agent to another
/// (tools-p1p2 Phase B).
///
/// The messenger is wired late by the gateway (same pattern as
/// [`crate::subagent::SubagentSpawner`]) because it needs the fully-built
/// `AgentRuntime`. Implementations trigger an asynchronous turn on the target
/// agent and return a delivery confirmation immediately; the turn's outcome is
/// logged, not awaited.
#[async_trait]
pub trait AgentMessenger: Send + Sync {
    /// Deliver `message` from `from_agent` to `to_agent`, triggering an
    /// asynchronous turn on the target. Returns a delivery confirmation string.
    async fn send(
        &self,
        from_agent: &str,
        to_agent: &str,
        message: &str,
    ) -> Result<String, MessengerError>;
}
