//! Versioned external event-bus types for the `/events` endpoint.
//!
//! These DTOs are a *stable, independently versioned* surface that external
//! tools and future GUIs consume. They are deliberately decoupled from the
//! internal runtime event stream (`RunEvent`) and the internal `/ws` protocol
//! (`WsFrame`): the runtime is free to evolve while the harness schema only
//! bumps [`HARNESS_API_VERSION`] on a semantic change. Conversion happens at
//! the gateway boundary via [`HarnessEvent::from_agent_payload`], which reads
//! the *already-serialized* `agent` event payload produced by
//! `legion_host::turn::run_event_to_payload` — so the host crate never has to
//! change to feed this bus.
//!
//! Design notes:
//! - Field names are `camelCase` (per AGENTS.md serde convention).
//! - `ToolCallView`/`ToolResultView` are small, independent shapes rather than
//!   re-exports of internal runtime types, so the public surface stays stable.
//! - History in [`HarnessServerFrame::Attached`] reuses `legion-provider`'s
//!   `ChatMessage` directly — it is already the single persisted source of
//!   truth on disk, so we don't model it twice.

use legion_provider::types::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Harness API version. Bumped on a breaking schema change. The server echoes
/// it back in [`HarnessServerFrame::HelloOk`] and rejects mismatched
/// [`HarnessRequest::Hello`] handshakes.
pub const HARNESS_API_VERSION: u32 = 1;

/// A read-only view of a tool call, matching the shape serialized into the
/// `agent` event payload by `run_event_to_payload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallView {
    pub id: String,
    pub name: String,
    /// Tool arguments as a JSON string (as carried on the wire).
    pub arguments: String,
}

/// A read-only view of a tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultView {
    pub content: String,
    pub is_error: bool,
}

/// A stable, external lifecycle/streaming event derived from the runtime.
///
/// v1 emits `RunStarted`/`RunFinished`/`RunErrored`/`AssistantTextDelta`/
/// `ToolStarted`/`ToolFinished`. `ContextCompacted` and `TodoListUpdated` are
/// reserved here (so the schema is forward-compatible) but are not emitted in
/// v1 — see [`HarnessEvent::from_agent_payload`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// A run (turn) was accepted and is starting. Emitted once per turn.
    RunStarted {
        #[serde(rename = "sessionKey")]
        session_key: String,
        #[serde(rename = "runId")]
        run_id: String,
    },
    /// A run completed normally. Emitted once per turn.
    RunFinished {
        #[serde(rename = "sessionKey")]
        session_key: String,
        #[serde(rename = "runId")]
        run_id: String,
    },
    /// A run failed. Emitted once per turn on error.
    RunErrored {
        #[serde(rename = "sessionKey")]
        session_key: String,
        #[serde(rename = "runId")]
        run_id: String,
        error: String,
    },
    /// A streaming text fragment from the assistant. Accumulate to rebuild the
    /// message; individual fragments are not content blocks.
    AssistantTextDelta {
        #[serde(rename = "runId")]
        run_id: String,
        delta: String,
    },
    /// A tool call is about to execute.
    ToolStarted {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "toolCall")]
        tool_call: ToolCallView,
    },
    /// A tool call finished with a result.
    ToolFinished {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "toolCall")]
        tool_call: ToolCallView,
        result: ToolResultView,
        /// Canonical tool metadata when the tool is in the registry; absent
        /// for permission-denied sub-agent calls.
        #[serde(rename = "canonicalMeta", skip_serializing_if = "Option::is_none")]
        canonical_meta: Option<Value>,
    },
    // --- v1 reserved (schema-visible, not yet emitted) ---
    #[allow(dead_code)]
    ContextCompacted {
        #[serde(rename = "runId")]
        run_id: String,
        summary: String,
        #[serde(rename = "tokensCompacted", skip_serializing_if = "Option::is_none")]
        tokens_compacted: Option<usize>,
    },
    #[allow(dead_code)]
    TodoListUpdated {
        #[serde(rename = "runId")]
        run_id: String,
        items: Value,
    },
}

impl HarnessEvent {
    /// Map an `agent` event payload (as built by
    /// `legion_host::turn::run_event_to_payload`) into a stable
    /// [`HarnessEvent`].
    ///
    /// The payload carries a `stream` discriminant plus `run_id`. v1 surfaces
    /// only run/assistant/tool events; compaction and todo payloads map to
    /// `None` (reserved for a later schema bump).
    ///
    /// Returns `None` if the payload is not a recognized agent event (e.g. an
    /// `approval`/`question` frame, or a malformed payload).
    pub fn from_agent_payload(session_key: &str, payload: &Value) -> Option<HarnessEvent> {
        let run_id = payload
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stream = payload.get("stream").and_then(Value::as_str)?;
        match stream {
            "lifecycle" => {
                let phase = payload.get("phase").and_then(Value::as_str)?;
                match phase {
                    "start" => Some(Self::RunStarted {
                        session_key: session_key.to_string(),
                        run_id,
                    }),
                    "end" => Some(Self::RunFinished {
                        session_key: session_key.to_string(),
                        run_id,
                    }),
                    "error" => {
                        let error = payload
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("run error")
                            .to_string();
                        Some(Self::RunErrored {
                            session_key: session_key.to_string(),
                            run_id,
                            error,
                        })
                    }
                    _ => None,
                }
            }
            "assistant" => {
                let delta = payload.get("delta").and_then(Value::as_str)?.to_string();
                Some(Self::AssistantTextDelta { run_id, delta })
            }
            "tool" => {
                // `state` is "start" | "end".
                let tool_call = payload
                    .get("tool_call")
                    .and_then(read_tool_call_view)
                    .unwrap_or(ToolCallView {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                match payload.get("state").and_then(Value::as_str)? {
                    "start" => Some(Self::ToolStarted { run_id, tool_call }),
                    "end" => {
                        let result = read_tool_result_view(payload);
                        let canonical_meta = payload.get("canonical_meta").cloned();
                        Some(Self::ToolFinished {
                            run_id,
                            tool_call,
                            result,
                            canonical_meta,
                        })
                    }
                    _ => None,
                }
            }
            // v1 reserved: compaction and todo_update are not surfaced yet.
            "compaction" | "todo_update" => None,
            _ => None,
        }
    }
}

fn read_tool_call_view(value: &Value) -> Option<ToolCallView> {
    Some(ToolCallView {
        id: value.get("id").and_then(Value::as_str)?.to_string(),
        name: value.get("name").and_then(Value::as_str)?.to_string(),
        arguments: value
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn read_tool_result_view(payload: &Value) -> ToolResultView {
    let result = payload.get("result");
    ToolResultView {
        content: result
            .and_then(|r| r.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        is_error: result
            .and_then(|r| r.get("is_error"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// A request sent by a `/events` client to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessRequest {
    /// Version handshake. Must be the first request after `connect`.
    Hello {
        v: u32,
    },
    /// List active (in-memory) sessions available to attach.
    ListSessions,
    /// Subscribe to a session's live event stream. Replaces any prior attach
    /// on this connection. `sessionKey` is the full 7-segment key
    /// (`agent:<agent_id>:<scope>:<channel>:<account_id>:<peer_kind>:<peer_id>`).
    AttachSession {
        #[serde(rename = "sessionKey")]
        session_key: String,
    },
    /// Stop subscribing to the current session, if any.
    DetachSession,
    Ping,
}

/// Liveness of an attached/attachable session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// A turn is currently running.
    Live,
    /// Registered but no turn in flight.
    Idle,
}

/// A session entry returned by [`HarnessRequest::ListSessions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSessionSummary {
    #[serde(rename = "sessionKey")]
    pub session_key: String,
    pub agent_id: String,
    pub peer_id: String,
    /// Current run id when `status` is `live`.
    #[serde(rename = "runId", skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub status: SessionStatus,
}

/// A frame sent by the server to a `/events` client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessServerFrame {
    /// Reply to [`HarnessRequest::Hello`], echoing the server's version.
    HelloOk { v: u32 },
    /// Generic acknowledgement (unused for most requests; reserved).
    Ok,
    /// Error. `message` is human-readable; the connection stays open unless it
    /// is a fatal handshake/loopback error.
    Error { message: String },
    /// Reply to [`HarnessRequest::ListSessions`].
    SessionList {
        sessions: Vec<HarnessSessionSummary>,
    },
    /// Reply to [`HarnessRequest::AttachSession`]. `history` is the persisted
    /// chat-message transcript (single source of truth on disk); `runId` is
    /// present when a turn is already running.
    Attached {
        #[serde(rename = "sessionKey")]
        session_key: String,
        #[serde(rename = "runId", skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        history: Vec<ChatMessage>,
    },
    /// Reply to [`HarnessRequest::DetachSession`].
    Detached,
    /// A streamed lifecycle/tool event for the attached session.
    Event { event: HarnessEvent },
    /// Reply to [`HarnessRequest::Ping`].
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION_KEY: &str = "agent:bot:dm:cli:acct:direct:peer1";

    #[test]
    fn server_frame_event_round_trips() {
        let frame = HarnessServerFrame::Event {
            event: HarnessEvent::AssistantTextDelta {
                run_id: "run-1".into(),
                delta: "hi".into(),
            },
        };
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "event");
        assert_eq!(v["event"]["kind"], "assistant_text_delta");
        assert_eq!(v["event"]["runId"], "run-1");
        assert_eq!(v["event"]["delta"], "hi");
        let back: HarnessServerFrame = serde_json::from_value(v).unwrap();
        assert!(matches!(back, HarnessServerFrame::Event { .. }));
    }

    #[test]
    fn run_event_variants_round_trip() {
        let cases = vec![
            HarnessEvent::RunStarted {
                session_key: SESSION_KEY.into(),
                run_id: "r".into(),
            },
            HarnessEvent::ToolFinished {
                run_id: "r".into(),
                tool_call: ToolCallView {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                result: ToolResultView {
                    content: "ok".into(),
                    is_error: false,
                },
                canonical_meta: None,
            },
        ];
        for ev in cases {
            let v = serde_json::to_value(&ev).unwrap();
            let back: HarnessEvent = serde_json::from_value(v).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn from_payload_lifecycle_start() {
        let payload = json!({"stream": "lifecycle", "phase": "start", "run_id": "run-9"});
        let ev = HarnessEvent::from_agent_payload(SESSION_KEY, &payload).unwrap();
        assert!(matches!(
            ev,
            HarnessEvent::RunStarted { ref session_key, run_id } if session_key == SESSION_KEY && run_id == "run-9"
        ));
    }

    #[test]
    fn from_payload_lifecycle_end() {
        let payload = json!({"stream": "lifecycle", "phase": "end", "run_id": "run-9"});
        let ev = HarnessEvent::from_agent_payload(SESSION_KEY, &payload).unwrap();
        assert!(matches!(ev, HarnessEvent::RunFinished { .. }));
    }

    #[test]
    fn from_payload_lifecycle_error() {
        let payload =
            json!({"stream": "lifecycle", "phase": "error", "error": "boom", "run_id": "run-9"});
        let ev = HarnessEvent::from_agent_payload(SESSION_KEY, &payload).unwrap();
        match ev {
            HarnessEvent::RunErrored { error, .. } => assert_eq!(error, "boom"),
            other => panic!("expected RunErrored, got {other:?}"),
        }
    }

    #[test]
    fn from_payload_assistant_delta() {
        let payload = json!({"stream": "assistant", "delta": "hello", "run_id": "run-1"});
        let ev = HarnessEvent::from_agent_payload(SESSION_KEY, &payload).unwrap();
        match ev {
            HarnessEvent::AssistantTextDelta { run_id, delta } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(delta, "hello");
            }
            other => panic!("expected AssistantTextDelta, got {other:?}"),
        }
    }

    #[test]
    fn from_payload_tool_start() {
        let payload = json!({
            "stream": "tool", "state": "start",
            "tool_call": {"id": "t1", "name": "read", "arguments": "{\"x\":1}"},
            "run_id": "run-1"
        });
        let ev = HarnessEvent::from_agent_payload(SESSION_KEY, &payload).unwrap();
        match ev {
            HarnessEvent::ToolStarted { tool_call, .. } => {
                assert_eq!(tool_call.name, "read");
                assert_eq!(tool_call.arguments, "{\"x\":1}");
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    #[test]
    fn from_payload_tool_end_with_and_without_meta() {
        let base = json!({
            "stream": "tool", "state": "end",
            "tool_call": {"id": "t1", "name": "write", "arguments": "{}"},
            "result": {"content": "done", "is_error": false},
            "run_id": "run-1"
        });
        // Without canonical_meta.
        let ev = HarnessEvent::from_agent_payload(SESSION_KEY, &base).unwrap();
        match ev {
            HarnessEvent::ToolFinished {
                canonical_meta,
                result,
                ..
            } => {
                assert!(canonical_meta.is_none());
                assert!(!result.is_error);
            }
            other => panic!("expected ToolFinished, got {other:?}"),
        }
        // With canonical_meta.
        let mut with_meta = base.clone();
        if let serde_json::Value::Object(map) = &mut with_meta {
            map.insert("canonical_meta".into(), json!({"name": "write"}));
        }
        let ev = HarnessEvent::from_agent_payload(SESSION_KEY, &with_meta).unwrap();
        match ev {
            HarnessEvent::ToolFinished { canonical_meta, .. } => {
                assert_eq!(canonical_meta.unwrap()["name"], "write");
            }
            other => panic!("expected ToolFinished, got {other:?}"),
        }
    }

    #[test]
    fn from_payload_compaction_and_todo_are_ignored_in_v1() {
        let compaction = json!({"stream": "compaction", "summary": "s", "run_id": "r"});
        let todo = json!({"stream": "todo_update", "items": [], "run_id": "r"});
        assert!(HarnessEvent::from_agent_payload(SESSION_KEY, &compaction).is_none());
        assert!(HarnessEvent::from_agent_payload(SESSION_KEY, &todo).is_none());
    }

    #[test]
    fn from_payload_unrecognized_returns_none() {
        assert!(
            HarnessEvent::from_agent_payload(SESSION_KEY, &json!({"stream": "nope"})).is_none()
        );
        assert!(HarnessEvent::from_agent_payload(SESSION_KEY, &json!({})).is_none());
    }

    #[test]
    fn harness_request_attach_round_trips() {
        let req = HarnessRequest::AttachSession {
            session_key: SESSION_KEY.into(),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["type"], "attach_session");
        assert_eq!(v["sessionKey"], SESSION_KEY);
        let back: HarnessRequest = serde_json::from_value(v).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn hello_request_uses_version_field() {
        let v = serde_json::to_value(HarnessRequest::Hello {
            v: HARNESS_API_VERSION,
        })
        .unwrap();
        assert_eq!(v["type"], "hello");
        assert_eq!(v["v"], HARNESS_API_VERSION);
    }
}
