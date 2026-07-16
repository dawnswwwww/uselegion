//! Turn lifecycle helpers: resume preparation, run-stream driving, and event
//! to payload conversion.
//!
//! These functions are transport-neutral: they are used by the Gateway's
//! WebSocket `agent` RPC and by embedded CLI hosts.

use crate::routing::Router;
use crate::session::SessionStore;
use futures::StreamExt;
use legion_protocol::{AgentAccepted, AgentParams, WsFrame};
use legion_provider::types::{
    ChatMessage as ProviderChatMessage, ChatRole, FunctionCall, ToolCall as ProviderToolCall,
};
use legion_runtime::types::{LifecyclePhase, RunEvent};
use legion_runtime::{ApprovalGate, Harness, QuestionGate, RunRequest, RunStream};
use serde_json::{Value, json};
use std::sync::Arc;

/// Resolve the session key, load + repair resumable history, and start an
/// agent run against an explicit runtime/config/router/store set.
///
/// Returns the run stream, the accepted metadata, and the resolved session
/// key (rebuilt with the bound agent id). `approval_gate` is attached to the
/// run when provided.
pub async fn prepare_run(
    runtime: &dyn Harness,
    config: &legion_core::config::Config,
    router: &Router,
    session_store: &SessionStore,
    mut params: AgentParams,
    approval_gate: Option<Arc<ApprovalGate>>,
    question_gate: Option<Arc<QuestionGate>>,
) -> Result<(RunStream, AgentAccepted, String), String> {
    let session_key = crate::routing::resolve_session_key(&params.session_key, router)
        .ok_or_else(|| format!("invalid session key: {}", params.session_key))?;

    let mut history = session_store.load_for_resume(&session_key).await;
    // Repair interrupted tool turns before resuming so provider tool-call
    // invariants hold (session-resume Phase B).
    let repair = crate::session::repair::recover_orphaned_tool_results(
        &mut history,
        config.sessions.orphan_policy,
    );
    if !repair.is_clean() {
        tracing::warn!(
            session_key = %session_key,
            orphan_tool_uses = repair.orphan_tool_uses,
            orphan_tool_results = repair.orphan_tool_results,
            drift = ?repair.drift,
            "repaired resume drift"
        );
    }
    params.history = history;

    let (stream, accepted) = start_agent_run(
        runtime,
        config,
        router,
        params,
        approval_gate,
        question_gate,
    )?;

    Ok((stream, accepted, session_key))
}

/// Start an agent run and return an event stream plus the accepted metadata.
///
/// The session key is parsed to obtain the channel/account/peer, the agent is
/// resolved via the configured bindings, and the session key is rebuilt with the
/// resolved agent id so that workspace/auth profiles align with the binding.
///
/// `approval_gate`, when provided, is attached to the run so tools with
/// `Approval::Prompt`/`Required` can ask the originating user.
pub fn start_agent_run(
    runtime: &dyn Harness,
    config: &legion_core::config::Config,
    router: &Router,
    params: AgentParams,
    approval_gate: Option<Arc<ApprovalGate>>,
    question_gate: Option<Arc<QuestionGate>>,
) -> Result<(RunStream, AgentAccepted), String> {
    let parts = crate::routing::parse_session_key(&params.session_key)
        .ok_or_else(|| format!("invalid session key: {}", params.session_key))?;

    let router_msg = build_router_message(&parts, &params.message.content);
    let agent_id = router.resolve_agent(&router_msg);
    let session_key = crate::routing::build_session_key(&agent_id, &parts);
    let model_ref = resolve_model(config, &agent_id);
    let run_id = params
        .idempotency_key
        .clone()
        .unwrap_or_else(|| format!("run-{}", uuid_like()));

    let mut request = RunRequest::new(session_key, agent_id, params.message.content, model_ref)
        .with_history(params.history)
        .with_dump_prompts(params.dump_prompts)
        .with_workspace_override(params.workspace);
    if let Some(gate) = approval_gate {
        request = request.with_approval_gate(gate);
    }
    if let Some(gate) = question_gate {
        request = request.with_question_gate(gate);
    }

    let stream = runtime.run(request).map_err(|e| e.to_string())?;

    let accepted = AgentAccepted {
        run_id,
        accepted_at: iso_now(),
    };

    Ok((stream, accepted))
}

fn build_router_message(
    parts: &crate::routing::SessionKeyParts,
    content: &str,
) -> legion_plugin_sdk::channel::InboundMessage {
    use legion_plugin_sdk::channel::{InboundMessage, Peer, Sender};
    InboundMessage {
        channel: parts.channel.clone(),
        account_id: parts.account_id.clone(),
        peer: Peer {
            kind: parts.peer_kind.clone(),
            id: parts.peer_id.clone(),
            name: None,
            thread_id: None,
        },
        sender: Sender {
            id: parts.peer_id.clone(),
            display_name: None,
            username: None,
        },
        message_id: "rpc".into(),
        text: Some(content.into()),
        media: vec![],
        reply_to: None,
        timestamp: iso_now(),
        is_mentioned: false,
        ambient: false,
        guild_id: None,
        team_id: None,
    }
}

fn resolve_model(config: &legion_core::config::Config, agent_id: &str) -> String {
    if agent_id == "main" {
        config.agents.defaults.model.clone()
    } else {
        config
            .agents
            .list
            .iter()
            .find(|a| a.id == agent_id)
            .and_then(|a| a.model.clone())
            .or_else(|| config.agents.defaults.model.clone())
    }
    .unwrap_or_else(|| "openai/gpt-4o".to_string())
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("run-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn iso_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}.{:03}Z", d.as_secs(), d.subsec_millis()))
        .unwrap_or_default()
}

/// Convert a runtime event into the payload used by the `agent` event frame.
pub fn run_event_to_payload(run_id: &str, event: &RunEvent) -> Value {
    let mut payload = match event {
        RunEvent::Lifecycle { phase, error } => {
            let mut p = json!({"stream": "lifecycle", "phase": phase});
            if let Some(e) = error {
                p.as_object_mut()
                    .unwrap()
                    .insert("error".to_string(), Value::String(e.clone()));
            }
            p
        }
        RunEvent::AssistantDelta { delta } => {
            json!({"stream": "assistant", "delta": delta})
        }
        RunEvent::ToolStart { tool_call } => {
            json!({"stream": "tool", "state": "start", "tool_call": tool_call})
        }
        RunEvent::ToolEnd {
            tool_call,
            result,
            canonical_meta,
        } => {
            let mut payload = json!({
                "stream": "tool",
                "state": "end",
                "tool_call": tool_call,
                "result": result
            });
            if let Some(meta) = canonical_meta {
                if let serde_json::Value::Object(map) = &mut payload {
                    map.insert(
                        "canonical_meta".to_string(),
                        serde_json::to_value(meta).unwrap_or_default(),
                    );
                }
            }
            payload
        }
        RunEvent::Compaction {
            summary, boundary, ..
        } => {
            let mut payload = json!({"stream": "compaction", "summary": summary});
            if let Some(b) = boundary {
                if let serde_json::Value::Object(map) = &mut payload {
                    map.insert("tokens_compacted".to_string(), json!(b.tokens_compacted));
                }
            }
            payload
        }
        RunEvent::TodoUpdate { list } => {
            json!({"stream": "todo_update", "items": list.items})
        }
    };

    if let Value::Object(ref mut map) = payload {
        map.insert("run_id".to_string(), Value::String(run_id.to_string()));
    }
    payload
}

/// Consume a run's event stream, persisting the transcript and forwarding
/// each event through `emit` as a `WsFrame::event("agent", payload)`.
///
/// Shared run driver behind the WS `agent` RPC: appends the user message up
/// front, persists compaction boundaries (plus their resume head) so
/// `load_for_resume` reconstructs the effective context, and appends the
/// accumulated conversation history once the run ends. The terminal
/// `Lifecycle::End` event is emitted last, after history is persisted.
pub async fn drive_run_stream(
    mut stream: RunStream,
    session_store: Arc<SessionStore>,
    session_key: String,
    user_content: String,
    run_id: String,
    mut emit: impl FnMut(WsFrame),
) {
    session_store
        .append(&session_key, &[ProviderChatMessage::user(user_content)])
        .await;
    let mut accumulator = SessionAccumulator::new();
    let mut end_event = None;
    while let Some(event) = stream.next().await {
        if matches!(
            event,
            RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                ..
            }
        ) {
            end_event = Some(event);
            break;
        }
        if let RunEvent::Compaction {
            boundary: Some(boundary),
            resume_head,
            ..
        } = &event
        {
            session_store.append_boundary(&session_key, boundary).await;
            // Persist the compacted head right after the boundary so
            // `load_for_resume` reconstructs the effective context from the
            // tail.
            session_store.append(&session_key, resume_head).await;
        }
        accumulator.on_event(&event);
        let payload = run_event_to_payload(&run_id, &event);
        emit(WsFrame::event("agent", payload));
    }
    let new_messages = accumulator.into_history();
    session_store.append(&session_key, &new_messages).await;
    if let Some(event) = end_event {
        let payload = run_event_to_payload(&run_id, &event);
        emit(WsFrame::event("agent", payload));
    }
}

/// Accumulates runtime events into a conversation history for a single run.
///
/// The runtime emits assistant text deltas, then tool-start/end pairs (one per
/// executed tool call), then possibly more assistant deltas for the next turn.
/// This accumulator keeps the assistant message (with its `tool_calls`) before
/// the matching tool-result messages, even though the tool-end events arrive
/// before the assistant message is complete.
#[derive(Default)]
pub struct SessionAccumulator {
    history: Vec<ProviderChatMessage>,
    current_assistant_text: String,
    current_tool_calls: Vec<ProviderToolCall>,
    pending_tool_results: Vec<ProviderChatMessage>,
    seen_tool_end_this_turn: bool,
}

impl SessionAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Flush the current assistant turn: emit the assistant message (text + any
    /// tool calls) followed by all pending tool results, then reset turn state.
    fn flush_turn(&mut self) {
        if !self.current_assistant_text.is_empty() || !self.current_tool_calls.is_empty() {
            let mut msg = ProviderChatMessage::assistant(&self.current_assistant_text);
            if !self.current_tool_calls.is_empty() {
                msg.tool_calls = Some(std::mem::take(&mut self.current_tool_calls));
            }
            self.history.push(msg);
            self.current_assistant_text.clear();
        }
        if !self.pending_tool_results.is_empty() {
            self.history
                .extend(std::mem::take(&mut self.pending_tool_results));
        }
        self.seen_tool_end_this_turn = false;
    }

    pub fn on_event(&mut self, event: &RunEvent) {
        match event {
            RunEvent::AssistantDelta { delta } => {
                // Any assistant text that arrives after tool results belongs to
                // the next turn; flush the previous turn first.
                if self.seen_tool_end_this_turn {
                    self.flush_turn();
                }
                self.current_assistant_text.push_str(delta);
            }
            RunEvent::ToolStart { tool_call } => {
                self.current_tool_calls.push(ProviderToolCall {
                    id: tool_call.id.clone(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: tool_call.name.clone(),
                        arguments: tool_call.arguments.clone(),
                    },
                });
            }
            RunEvent::ToolEnd {
                tool_call, result, ..
            } => {
                self.seen_tool_end_this_turn = true;
                self.pending_tool_results.push(ProviderChatMessage {
                    role: ChatRole::Tool,
                    content: result.content.clone(),
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(tool_call.id.clone()),
                    cache_breakpoint: false,
                });
            }
            RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                ..
            } => {
                self.flush_turn();
            }
            _ => {}
        }
    }

    pub fn into_history(mut self) -> Vec<ProviderChatMessage> {
        self.flush_turn();
        self.history
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::tools::{ToolCall as RuntimeToolCall, ToolResult};

    fn runtime_tool(id: &str, name: &str, args: &str) -> RuntimeToolCall {
        RuntimeToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[test]
    fn text_only_run_emits_single_assistant_message() {
        let mut acc = SessionAccumulator::new();
        acc.on_event(&RunEvent::AssistantDelta {
            delta: "hello".to_string(),
        });
        acc.on_event(&RunEvent::Lifecycle {
            phase: LifecyclePhase::End,
            error: None,
        });
        let hist = acc.into_history();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].role, ChatRole::Assistant);
        assert_eq!(hist[0].content, "hello");
    }

    #[test]
    fn assistant_message_precedes_tool_results() {
        let mut acc = SessionAccumulator::new();
        acc.on_event(&RunEvent::AssistantDelta {
            delta: "<think>use exec</think>".to_string(),
        });
        acc.on_event(&RunEvent::ToolStart {
            tool_call: runtime_tool("call_1", "exec", r#"{"cmd":"ls"}"#),
        });
        acc.on_event(&RunEvent::ToolEnd {
            tool_call: runtime_tool("call_1", "exec", r#"{"cmd":"ls"}"#),
            result: ToolResult::ok("file1\nfile2\n"),
            canonical_meta: None,
        });
        acc.on_event(&RunEvent::Lifecycle {
            phase: LifecyclePhase::End,
            error: None,
        });
        let hist = acc.into_history();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].role, ChatRole::Assistant);
        assert!(hist[0].tool_calls.is_some());
        assert_eq!(hist[0].tool_calls.as_ref().unwrap()[0].id, "call_1");
        assert_eq!(hist[1].role, ChatRole::Tool);
        assert_eq!(hist[1].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn multiple_tool_calls_grouped_before_results() {
        let mut acc = SessionAccumulator::new();
        acc.on_event(&RunEvent::AssistantDelta {
            delta: "<think>run two commands</think>".to_string(),
        });
        acc.on_event(&RunEvent::ToolStart {
            tool_call: runtime_tool("call_1", "exec", r#"{"cmd":"pwd"}"#),
        });
        acc.on_event(&RunEvent::ToolEnd {
            tool_call: runtime_tool("call_1", "exec", r#"{"cmd":"pwd"}"#),
            result: ToolResult::ok("/home"),
            canonical_meta: None,
        });
        acc.on_event(&RunEvent::ToolStart {
            tool_call: runtime_tool("call_2", "exec", r#"{"cmd":"ls"}"#),
        });
        acc.on_event(&RunEvent::ToolEnd {
            tool_call: runtime_tool("call_2", "exec", r#"{"cmd":"ls"}"#),
            result: ToolResult::ok("file1"),
            canonical_meta: None,
        });
        acc.on_event(&RunEvent::Lifecycle {
            phase: LifecyclePhase::End,
            error: None,
        });
        let hist = acc.into_history();
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].role, ChatRole::Assistant);
        let calls = hist[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(hist[1].role, ChatRole::Tool);
        assert_eq!(hist[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(hist[2].role, ChatRole::Tool);
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("call_2"));
    }

    #[test]
    fn text_after_tool_results_starts_new_turn() {
        let mut acc = SessionAccumulator::new();
        acc.on_event(&RunEvent::AssistantDelta {
            delta: "<think>exec</think>".to_string(),
        });
        acc.on_event(&RunEvent::ToolStart {
            tool_call: runtime_tool("call_1", "exec", r#"{"cmd":"ls"}"#),
        });
        acc.on_event(&RunEvent::ToolEnd {
            tool_call: runtime_tool("call_1", "exec", r#"{"cmd":"ls"}"#),
            result: ToolResult::ok("file1"),
            canonical_meta: None,
        });
        acc.on_event(&RunEvent::AssistantDelta {
            delta: "Done.".to_string(),
        });
        acc.on_event(&RunEvent::Lifecycle {
            phase: LifecyclePhase::End,
            error: None,
        });
        let hist = acc.into_history();
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].role, ChatRole::Assistant);
        assert_eq!(hist[1].role, ChatRole::Tool);
        assert_eq!(hist[2].role, ChatRole::Assistant);
        assert_eq!(hist[2].content, "Done.");
    }

    #[test]
    fn compaction_payload_includes_tokens_only_with_boundary() {
        use legion_runtime::types::BoundaryMark;
        let with_boundary = RunEvent::Compaction {
            summary: "summed up".to_string(),
            boundary: Some(BoundaryMark {
                entry_index: 3,
                timestamp_iso: "2026-07-11T12:00:00.000Z".to_string(),
                tokens_compacted: 456,
            }),
            resume_head: vec![],
        };
        let payload = run_event_to_payload("run-1", &with_boundary);
        assert_eq!(payload["stream"], "compaction");
        assert_eq!(payload["summary"], "summed up");
        assert_eq!(payload["tokens_compacted"], 456);
        assert_eq!(payload["run_id"], "run-1");

        let without_boundary = RunEvent::Compaction {
            summary: "summed up".to_string(),
            boundary: None,
            resume_head: vec![],
        };
        let payload = run_event_to_payload("run-1", &without_boundary);
        assert_eq!(payload["stream"], "compaction");
        assert!(payload.get("tokens_compacted").is_none());
        assert_eq!(payload["run_id"], "run-1");
    }

    #[test]
    fn lifecycle_error_payload_serializes_error() {
        use legion_runtime::types::LifecyclePhase;
        let event = RunEvent::Lifecycle {
            phase: LifecyclePhase::Error,
            error: Some("boom".to_string()),
        };
        let payload = run_event_to_payload("run-2", &event);
        assert_eq!(payload["stream"], "lifecycle");
        assert_eq!(payload["phase"], "error");
        assert_eq!(payload["error"], "boom");
        assert_eq!(payload["run_id"], "run-2");

        // Without an error the key is absent.
        let event = RunEvent::Lifecycle {
            phase: LifecyclePhase::End,
            error: None,
        };
        let payload = run_event_to_payload("run-2", &event);
        assert!(payload.get("error").is_none());
        assert_eq!(payload["run_id"], "run-2");
    }

    #[test]
    fn every_payload_carries_run_id() {
        use legion_runtime::tools::{ToolCall, ToolResult};

        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        let events = vec![
            RunEvent::Lifecycle {
                phase: LifecyclePhase::Start,
                error: None,
            },
            RunEvent::AssistantDelta {
                delta: "hi".to_string(),
            },
            RunEvent::ToolStart {
                tool_call: tool_call.clone(),
            },
            RunEvent::ToolEnd {
                tool_call,
                result: ToolResult::ok("done"),
                canonical_meta: None,
            },
            RunEvent::Compaction {
                summary: "s".to_string(),
                boundary: None,
                resume_head: vec![],
            },
        ];
        for event in &events {
            let payload = run_event_to_payload("run-xyz", event);
            assert_eq!(
                payload["run_id"], "run-xyz",
                "payload for {event:?} must carry run_id"
            );
        }
    }
}
