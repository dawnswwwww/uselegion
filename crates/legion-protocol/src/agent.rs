//! Shared agent RPC DTOs used by both the Gateway and the CLI.

use legion_provider::types::ChatMessage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Parameters for the `agent` method.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentParams {
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub message: UserMessage,
    #[serde(default, rename = "idempotencyKey")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub wait: bool,
    #[serde(default)]
    pub history: Vec<ChatMessage>,
    /// Dump the assembled system prompt for this run to
    /// `~/.legion/dump-prompts/<session>.jsonl`.
    #[serde(default, rename = "dumpPrompts")]
    pub dump_prompts: bool,
    /// Yolo mode: auto-approve every tool prompt without asking the user
    /// (`legion agent --yolo`). Hard policy denies still apply.
    #[serde(default)]
    pub yolo: bool,
    /// Per-run workspace override (embedded CLI only: `--workspace` / cwd
    /// default). The gateway WS path never sends this (`skip_serializing_if`),
    /// so deserialized values are always `None` on the gateway side — it keeps
    /// resolving workspace from its own config. Only affects the "working"
    /// layer (tools/bootstrap/skills); the memory backend is unaffected.
    #[serde(default, rename = "workspace", skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserMessage {
    pub role: String,
    pub content: String,
}

/// Response payload for an accepted agent run.
#[derive(Debug, Clone, Serialize)]
pub struct AgentAccepted {
    pub run_id: String,
    pub accepted_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_params_yolo_defaults_to_false_and_parses() {
        let minimal: AgentParams = serde_json::from_str(
            r#"{
                "sessionKey": "agent:main:dm:cli:default:direct:cli",
                "message": { "role": "user", "content": "hi" }
            }"#,
        )
        .unwrap();
        assert!(!minimal.yolo, "yolo must default to false");
        assert!(!minimal.dump_prompts);
        assert!(!minimal.wait);
        assert!(minimal.history.is_empty());
        assert!(
            minimal.workspace.is_none(),
            "workspace must default to None"
        );

        let yolo: AgentParams = serde_json::from_str(
            r#"{
                "sessionKey": "agent:main:dm:cli:default:direct:cli",
                "message": { "role": "user", "content": "hi" },
                "yolo": true,
                "dumpPrompts": true,
                "wait": true,
                "history": [{"role": "assistant", "content": "prev"}]
            }"#,
        )
        .unwrap();
        assert!(yolo.yolo);
        assert!(yolo.dump_prompts);
        assert!(yolo.wait);
        assert_eq!(yolo.history.len(), 1);
    }

    #[test]
    fn user_message_round_trip() {
        let msg = UserMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: UserMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, "user");
        assert_eq!(parsed.content, "hello");
    }
}
