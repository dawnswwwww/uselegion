//! Session self-inspection tools (tools-p1p2 gap, Phase A).
//!
//! Three read-only tools that let an agent inspect its own session
//! transcripts:
//!
//! - `session_status`: aggregate stats for the current session.
//! - `sessions_list`: lite summaries of every peer session of the agent.
//! - `sessions_history`: paged message history of one peer session.
//!
//! Permission boundary (gap doc §6.6): every tool only ever accesses
//! sessions of `ctx.agent_id`. Inputs carry no agent parameter by design,
//! and `session_status` rejects session keys whose agent segment does not
//! match the calling agent.

use std::sync::Arc;

use async_trait::async_trait;
use legion_provider::types::ChatRole;
use legion_runtime::tools::{Policy, Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::session::SessionStore;

const DEFAULT_LIST_LIMIT: usize = 20;
const MAX_LIST_LIMIT: usize = 100;
const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 200;
const MAX_MESSAGE_CHARS: usize = 2000;

/// Parse a session key (`agent:<agent>:<scope>:<channel>:<account>:<kind>:<peer>`)
/// into `(agent_id, peer_id)`.
fn parse_session_key(session_key: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = session_key.split(':').collect();
    if parts.len() != 7 || parts[0] != "agent" {
        return None;
    }
    Some((parts[1].to_string(), parts[6].to_string()))
}

/// Peer ids map directly to transcript file names (`<peer>.jsonl`), so they
/// must not contain path separators or other special characters.
pub fn is_safe_peer_id(peer_id: &str) -> bool {
    !peer_id.is_empty()
        && peer_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn usize_param(params: &Value, key: &str, default: usize, max: usize) -> usize {
    params
        .get(key)
        .and_then(Value::as_u64)
        .map(|v| (v as usize).min(max))
        .unwrap_or(default.min(max))
}

fn role_str(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

/// `session_status`: aggregate statistics for the current session.
pub struct SessionStatusTool {
    store: Arc<SessionStore>,
    policy: Policy,
}

impl SessionStatusTool {
    pub fn new(store: Arc<SessionStore>, policy: Policy) -> Self {
        Self { store, policy }
    }
}

#[async_trait]
impl Tool for SessionStatusTool {
    fn name(&self) -> &str {
        "session_status"
    }

    fn description(&self) -> &str {
        "Show aggregate statistics for the current session: entry and message \
         counts, compaction boundary count, last activity timestamp, and \
         transcript file size."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, _params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let Some((agent_id, peer_id)) = parse_session_key(&ctx.session_id) else {
            return Ok(ToolResult::error(format!(
                "invalid session key: {}",
                ctx.session_id
            )));
        };
        if agent_id != ctx.agent_id {
            return Ok(ToolResult::error(
                "cross-agent session access denied".to_string(),
            ));
        }
        let Some(stats) = self.store.stats(&ctx.session_id).await else {
            return Ok(ToolResult::error(format!(
                "session transcript not found: {}",
                ctx.session_id
            )));
        };
        let out = json!({
            "sessionKey": ctx.session_id,
            "agentId": agent_id,
            "peerId": peer_id,
            "entries": stats.entries,
            "userMessages": stats.user_messages,
            "assistantMessages": stats.assistant_messages,
            "toolMessages": stats.tool_messages,
            "systemMessages": stats.system_messages,
            "boundaryMarks": stats.boundary_marks,
            "lastTimestamp": stats.last_timestamp,
            "fileBytes": stats.file_bytes,
        });
        Ok(ToolResult::ok(out.to_string()))
    }
}

/// `sessions_list`: lite summaries of the agent's peer sessions.
pub struct SessionsListTool {
    store: Arc<SessionStore>,
    policy: Policy,
    lite_read_buffer_bytes: usize,
}

impl SessionsListTool {
    pub fn new(store: Arc<SessionStore>, policy: Policy, lite_read_buffer_bytes: usize) -> Self {
        Self {
            store,
            policy,
            lite_read_buffer_bytes,
        }
    }
}

#[async_trait]
impl Tool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List the current agent's stored sessions with their first user \
         prompt. Read-only; results are sorted by peer id and capped by \
         `limit` (default 20, max 100)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of sessions to return (default 20, max 100).",
                    "minimum": 0,
                    "maximum": MAX_LIST_LIMIT
                }
            },
            "additionalProperties": false
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let limit = usize_param(&params, "limit", DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT);
        let mut summaries = self
            .store
            .list_session_summaries(&ctx.agent_id, self.lite_read_buffer_bytes)
            .await;
        summaries.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        summaries.truncate(limit);
        let out: Vec<Value> = summaries
            .iter()
            .map(|s| {
                json!({
                    "peerId": s.peer_id,
                    "firstPrompt": s.first_prompt,
                    "truncated": s.truncated,
                })
            })
            .collect();
        Ok(ToolResult::ok(Value::Array(out).to_string()))
    }
}

/// `sessions_history`: paged message history of one of the agent's sessions.
pub struct SessionsHistoryTool {
    store: Arc<SessionStore>,
    policy: Policy,
}

impl SessionsHistoryTool {
    pub fn new(store: Arc<SessionStore>, policy: Policy) -> Self {
        Self { store, policy }
    }
}

#[async_trait]
impl Tool for SessionsHistoryTool {
    fn name(&self) -> &str {
        "sessions_history"
    }

    fn description(&self) -> &str {
        "Read messages from one of the current agent's sessions. `peerId` \
         defaults to the current session's peer; `offset`/`limit` page the \
         result (default limit 50, max 200). Message content is truncated to \
         2000 characters."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "peerId": {
                    "type": "string",
                    "description": "Peer session to read (defaults to the current session's peer)."
                },
                "offset": {
                    "type": "integer",
                    "description": "Number of leading messages to skip (default 0).",
                    "minimum": 0
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of messages to return (default 50, max 200).",
                    "minimum": 0,
                    "maximum": MAX_HISTORY_LIMIT
                }
            },
            "additionalProperties": false
        })
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let peer_id = match params.get("peerId").and_then(Value::as_str) {
            Some(peer) => peer.to_string(),
            None => match parse_session_key(&ctx.session_id) {
                Some((_, peer)) => peer,
                None => {
                    return Ok(ToolResult::error(format!(
                        "invalid session key: {}",
                        ctx.session_id
                    )));
                }
            },
        };
        if !is_safe_peer_id(&peer_id) {
            return Ok(ToolResult::error(format!(
                "invalid peerId: {peer_id} (allowed: letters, digits, '.', '_', '-')"
            )));
        }
        let offset = usize_param(&params, "offset", 0, usize::MAX);
        let limit = usize_param(&params, "limit", DEFAULT_HISTORY_LIMIT, MAX_HISTORY_LIMIT);

        let Some(messages) = self
            .store
            .transcript_messages(&ctx.agent_id, &peer_id)
            .await
        else {
            return Ok(ToolResult::error(format!(
                "session transcript not found for peer: {peer_id}"
            )));
        };

        let total = messages.len();
        let page: Vec<Value> = messages
            .iter()
            .skip(offset)
            .take(limit)
            .map(|m| {
                let content: String = m.content.chars().take(MAX_MESSAGE_CHARS).collect();
                json!({
                    "role": role_str(&m.role),
                    "content": content,
                    "hasToolCalls": m.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty()),
                })
            })
            .collect();
        let out = json!({
            "peerId": peer_id,
            "total": total,
            "offset": offset,
            "messages": page,
        });
        Ok(ToolResult::ok(out.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_provider::types::{ChatMessage, ChatRole, FunctionCall, ToolCall};
    use legion_runtime::types::BoundaryMark;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn policy() -> Policy {
        Policy {
            approval: legion_runtime::tools::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    fn store() -> (Arc<SessionStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        (Arc::new(SessionStore::new(dir.path())), dir)
    }

    fn session_key(agent_id: &str, peer_id: &str) -> String {
        format!("agent:{agent_id}:dm:webchat:default:direct:{peer_id}")
    }

    fn ctx(agent_id: &str, session_key: &str) -> ToolContext {
        ToolContext {
            workspace: PathBuf::from("/tmp"),
            session_id: session_key.to_string(),
            agent_id: agent_id.to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: None,
            swarm: None,
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
            plan_mode_tracker: None,
        }
    }

    fn tool_message(content: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Tool,
            content: content.to_string(),
            name: None,
            tool_calls: None,
            tool_call_id: Some("call-1".to_string()),
            cache_breakpoint: false,
        }
    }

    fn assistant_with_tool_call(content: &str) -> ChatMessage {
        let mut msg = ChatMessage::assistant(content);
        msg.tool_calls = Some(vec![ToolCall {
            id: "call-1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        }]);
        msg
    }

    // ---- session_status ----

    #[tokio::test]
    async fn session_status_reports_stats() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store
            .append(
                &key,
                &[
                    ChatMessage::user("hi"),
                    assistant_with_tool_call("looking"),
                    tool_message("file contents"),
                ],
            )
            .await;
        store
            .append_boundary(
                &key,
                &BoundaryMark {
                    entry_index: 0,
                    timestamp_iso: "2026-07-11T12:00:00.000Z".to_string(),
                    tokens_compacted: 42,
                },
            )
            .await;
        store.append(&key, &[ChatMessage::user("again")]).await;

        let tool = SessionStatusTool::new(store, policy());
        let result = tool.execute(json!({}), ctx("main", &key)).await.unwrap();
        assert!(!result.is_error, "{}", result.content);
        let out: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(out["agentId"], "main");
        assert_eq!(out["peerId"], "user1");
        assert_eq!(out["entries"], 5);
        assert_eq!(out["userMessages"], 2);
        assert_eq!(out["assistantMessages"], 1);
        assert_eq!(out["toolMessages"], 1);
        assert_eq!(out["systemMessages"], 0);
        assert_eq!(out["boundaryMarks"], 1);
        assert!(out["lastTimestamp"].as_u64().is_some());
        assert!(out["fileBytes"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn session_status_denies_cross_agent_key() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store.append(&key, &[ChatMessage::user("hi")]).await;

        let tool = SessionStatusTool::new(store, policy());
        let result = tool.execute(json!({}), ctx("other", &key)).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("cross-agent"));
    }

    #[tokio::test]
    async fn session_status_missing_session_errors() {
        let (store, _dir) = store();
        let tool = SessionStatusTool::new(store, policy());
        let key = session_key("main", "ghost");
        let result = tool.execute(json!({}), ctx("main", &key)).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn session_status_invalid_key_errors() {
        let (store, _dir) = store();
        let tool = SessionStatusTool::new(store, policy());
        let result = tool
            .execute(json!({}), ctx("main", "not-a-session-key"))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("invalid session key"));
    }

    // ---- sessions_list ----

    #[tokio::test]
    async fn sessions_list_lists_sorted_and_limited() {
        let (store, _dir) = store();
        for peer in ["charlie", "alice", "bob"] {
            store
                .append(
                    &session_key("main", peer),
                    &[ChatMessage::user(format!("hi {peer}"))],
                )
                .await;
        }

        let tool = SessionsListTool::new(store, policy(), 65_536);
        let result = tool
            .execute(
                json!({"limit": 2}),
                ctx("main", &session_key("main", "alice")),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let out: Value = serde_json::from_str(&result.content).unwrap();
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["peerId"], "alice");
        assert_eq!(arr[0]["firstPrompt"], "hi alice");
        assert_eq!(arr[0]["truncated"], false);
        assert_eq!(arr[1]["peerId"], "bob");
    }

    #[tokio::test]
    async fn sessions_list_empty_dir_returns_empty_array() {
        let (store, _dir) = store();
        let tool = SessionsListTool::new(store, policy(), 65_536);
        let result = tool
            .execute(json!({}), ctx("main", &session_key("main", "user1")))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, "[]");
    }

    // ---- sessions_history ----

    async fn seed_history(store: &SessionStore, agent: &str, peer: &str, n: usize) {
        for i in 0..n {
            store
                .append(
                    &session_key(agent, peer),
                    &[
                        ChatMessage::user(format!("question {i}")),
                        ChatMessage::assistant(format!("answer {i}")),
                    ],
                )
                .await;
        }
    }

    #[tokio::test]
    async fn sessions_history_defaults_to_current_peer() {
        let (store, _dir) = store();
        seed_history(&store, "main", "user1", 2).await;
        seed_history(&store, "main", "user2", 1).await;

        let tool = SessionsHistoryTool::new(store, policy());
        let key = session_key("main", "user1");
        let result = tool.execute(json!({}), ctx("main", &key)).await.unwrap();
        assert!(!result.is_error, "{}", result.content);
        let out: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(out["peerId"], "user1");
        assert_eq!(out["total"], 4);
        assert_eq!(out["offset"], 0);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "question 0");
        assert_eq!(msgs[0]["hasToolCalls"], false);
    }

    #[tokio::test]
    async fn sessions_history_applies_offset_and_limit() {
        let (store, _dir) = store();
        seed_history(&store, "main", "user1", 3).await;

        let tool = SessionsHistoryTool::new(store, policy());
        let key = session_key("main", "user1");
        let result = tool
            .execute(json!({"offset": 1, "limit": 2}), ctx("main", &key))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(out["total"], 6);
        assert_eq!(out["offset"], 1);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], "answer 0");
        assert_eq!(msgs[1]["content"], "question 1");
    }

    #[tokio::test]
    async fn sessions_history_marks_tool_calls_and_truncates_content() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store
            .append(
                &key,
                &[
                    assistant_with_tool_call("working"),
                    ChatMessage::user("x".repeat(3000)),
                ],
            )
            .await;

        let tool = SessionsHistoryTool::new(store, policy());
        let result = tool.execute(json!({}), ctx("main", &key)).await.unwrap();
        let out: Value = serde_json::from_str(&result.content).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["hasToolCalls"], true);
        assert_eq!(
            msgs[1]["content"].as_str().unwrap().len(),
            MAX_MESSAGE_CHARS
        );
    }

    #[tokio::test]
    async fn sessions_history_reads_other_peer_of_same_agent() {
        let (store, _dir) = store();
        seed_history(&store, "main", "user2", 1).await;

        let tool = SessionsHistoryTool::new(store, policy());
        let key = session_key("main", "user1");
        let result = tool
            .execute(json!({"peerId": "user2"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let out: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(out["peerId"], "user2");
        assert_eq!(out["total"], 2);
    }

    #[tokio::test]
    async fn sessions_history_rejects_path_traversal() {
        let (store, _dir) = store();
        let tool = SessionsHistoryTool::new(store, policy());
        let key = session_key("main", "user1");
        for bad in ["../x", "a/b", "..\\x", ""] {
            let result = tool
                .execute(json!({"peerId": bad}), ctx("main", &key))
                .await
                .unwrap();
            assert!(result.is_error, "peerId {bad:?} must be rejected");
            assert!(result.content.contains("invalid peerId"));
        }
    }

    #[tokio::test]
    async fn sessions_history_missing_file_errors() {
        let (store, _dir) = store();
        let tool = SessionsHistoryTool::new(store, policy());
        let key = session_key("main", "user1");
        let result = tool
            .execute(json!({"peerId": "ghost"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn sessions_history_never_leaks_other_agent_transcripts() {
        let (store, _dir) = store();
        // A transcript exists for agent "other" with the same peer id; the
        // tool must only read within ctx.agent_id's directory.
        seed_history(&store, "other", "user1", 1).await;

        let tool = SessionsHistoryTool::new(store, policy());
        let key = session_key("main", "user1");
        let result = tool
            .execute(json!({"peerId": "user1"}), ctx("main", &key))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }
}
