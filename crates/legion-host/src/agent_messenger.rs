//! Agent-to-agent messenger (tools-p1p2 gap, Phase B).
//!
//! `RuntimeAgentMessenger` is the host-side implementation of
//! [`legion_runtime::AgentMessenger`]: it validates the delivery against the
//! target agent's `allowFrom` list, then spawns a background turn on the
//! shared `AgentRuntime` and returns a delivery confirmation immediately.
//!
//! Authorization model (gap doc §4.2/§6.3): the *target* agent opts in by
//! listing sender agent ids in `agents.list[].allowFrom`. An empty list (the
//! default) denies every sender — cross-agent messaging is off unless
//! explicitly configured.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use legion_core::config::Config;
use legion_runtime::messenger::{AgentMessenger, MessengerError};
use legion_runtime::{AgentRuntime, LifecyclePhase, RunEvent, RunRequest};

/// MVP default model reference, same as the channel inbound router
/// (`legion_channel::route_inbound_to_runtime`).
const A2A_MODEL_REF: &str = "openai/gpt-4o";

/// Check whether `from` may deliver a message to `to` under `config`.
///
/// Pure policy function: the target must exist in `agents.list`, and its
/// `allowFrom` must contain the sender. An empty `allowFrom` denies all.
pub fn check_allowed(config: &Config, from: &str, to: &str) -> Result<(), MessengerError> {
    let target = config
        .agents
        .list
        .iter()
        .find(|a| a.id == to)
        .ok_or_else(|| MessengerError::UnknownAgent(to.to_string()))?;

    if target.allow_from.iter().any(|allowed| allowed == from) {
        Ok(())
    } else {
        Err(MessengerError::NotAllowed {
            from: from.to_string(),
            to: to.to_string(),
        })
    }
}

/// [`AgentMessenger`] backed by the in-process [`AgentRuntime`]. Each
/// delivery spawns a detached turn on the target agent with session key
/// `agent:<to>:a2a:<from>`; the turn's events are only logged.
pub struct RuntimeAgentMessenger {
    runtime: Arc<AgentRuntime>,
    config: Config,
}

impl RuntimeAgentMessenger {
    pub fn new(runtime: Arc<AgentRuntime>, config: Config) -> Self {
        Self { runtime, config }
    }
}

#[async_trait]
impl AgentMessenger for RuntimeAgentMessenger {
    async fn send(
        &self,
        from_agent: &str,
        to_agent: &str,
        message: &str,
    ) -> Result<String, MessengerError> {
        check_allowed(&self.config, from_agent, to_agent)?;

        let session_key = format!("agent:{to_agent}:a2a:{from_agent}");
        let request = RunRequest::new(
            session_key.clone(),
            to_agent.to_string(),
            format!("[agent:{from_agent}] {message}"),
            A2A_MODEL_REF.to_string(),
        )
        // Background delivery: approvals fail closed instead of waiting on a
        // human, same as sub-agent runs.
        .with_interactive(false)
        .with_sender(format!("agent:{from_agent}"));

        let runtime = self.runtime.clone();
        let to = to_agent.to_string();
        let from = from_agent.to_string();
        tokio::spawn(async move {
            match runtime.run(request) {
                Ok(stream) => {
                    let mut failed: Option<String> = None;
                    tokio::pin!(stream);
                    while let Some(event) = stream.next().await {
                        if let RunEvent::Lifecycle {
                            phase: LifecyclePhase::Error,
                            error,
                        } = &event
                        {
                            failed = error.clone();
                            break;
                        }
                    }
                    match failed {
                        None => tracing::info!(
                            from = %from,
                            to = %to,
                            session = %session_key,
                            "agent-to-agent turn completed"
                        ),
                        Some(err) => tracing::warn!(
                            from = %from,
                            to = %to,
                            session = %session_key,
                            error = %err,
                            "agent-to-agent turn failed"
                        ),
                    }
                }
                Err(err) => tracing::warn!(
                    from = %from,
                    to = %to,
                    session = %session_key,
                    error = %err,
                    "agent-to-agent turn failed to start"
                ),
            }
        });

        Ok(format!("delivered to {to_agent} (async)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_agents(json_agents: &str) -> Config {
        Config::from_json(&format!(
            r#"{{ "gateway": {{ "auth": {{ "token": "x" }} }}, "agents": {{ "list": {json_agents} }} }}"#
        ))
        .expect("test config parses")
    }

    #[test]
    fn check_allowed_rejects_unknown_agent() {
        let cfg = config_with_agents(r#"[{ "id": "researcher", "allowFrom": ["main"] }]"#);
        let err = check_allowed(&cfg, "main", "ghost").unwrap_err();
        assert!(matches!(err, MessengerError::UnknownAgent(ref id) if id == "ghost"));
    }

    #[test]
    fn check_allowed_empty_allow_from_denies_all() {
        let cfg = config_with_agents(r#"[{ "id": "researcher" }]"#);
        let err = check_allowed(&cfg, "main", "researcher").unwrap_err();
        assert!(matches!(
            err,
            MessengerError::NotAllowed { ref from, ref to }
                if from == "main" && to == "researcher"
        ));
    }

    #[test]
    fn check_allowed_allows_listed_sender() {
        let cfg =
            config_with_agents(r#"[{ "id": "researcher", "allowFrom": ["main", "critic"] }]"#);
        assert!(check_allowed(&cfg, "main", "researcher").is_ok());
        assert!(check_allowed(&cfg, "critic", "researcher").is_ok());
        assert!(check_allowed(&cfg, "intruder", "researcher").is_err());
    }
}
