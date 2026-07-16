//! Orphan tool-call repair and resume consistency checks (session-resume
//! Phase B).
//!
//! A transcript interrupted mid tool-execution can violate provider API
//! invariants on resume: an assistant message may carry tool calls that never
//! produced results (the turn was cut off), or — with parallel tool batches —
//! a `tool` result may reference a call that is not in the history. These
//! helpers detect and repair that drift before the history is handed back to
//! the runtime.

use legion_core::config::OrphanPolicy;
use legion_provider::types::{ChatMessage, ChatRole};
use std::collections::HashSet;

/// Content used for synthesized results of interrupted tool calls.
pub const INTERRUPTED_PLACEHOLDER: &str = "[interrupted]";

/// Drift found (and, for the repair path, fixed) in a resumed history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsistencyReport {
    /// Tool calls without a matching result.
    pub orphan_tool_uses: usize,
    /// Tool results without a matching call.
    pub orphan_tool_results: usize,
    /// Assistant messages with neither content nor tool calls.
    pub empty_assistant: usize,
    /// Human-readable drift descriptions / repair actions taken.
    pub drift: Vec<String>,
}

impl ConsistencyReport {
    /// Whether the history had no drift at all.
    pub fn is_clean(&self) -> bool {
        self.orphan_tool_uses == 0
            && self.orphan_tool_results == 0
            && self.empty_assistant == 0
            && self.drift.is_empty()
    }
}

/// Inspect a resumed history without modifying it.
pub fn check_resume_consistency(msgs: &[ChatMessage]) -> ConsistencyReport {
    let use_ids = collect_tool_use_ids(msgs);
    let answered: HashSet<&str> = msgs
        .iter()
        .filter(|m| m.role == ChatRole::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let mut report = ConsistencyReport::default();

    for msg in msgs {
        match msg.role {
            ChatRole::Assistant => {
                let calls = msg.tool_calls.as_deref().unwrap_or(&[]);
                if calls.is_empty() && msg.content.trim().is_empty() {
                    report.empty_assistant += 1;
                    report
                        .drift
                        .push("assistant message with neither content nor tool calls".into());
                }
                for call in calls {
                    if !answered.contains(call.id.as_str()) {
                        report.orphan_tool_uses += 1;
                        report
                            .drift
                            .push(format!("tool_use {} has no tool_result", call.id));
                    }
                }
            }
            ChatRole::Tool => {
                let id = msg.tool_call_id.as_deref().unwrap_or("");
                if !use_ids.contains(id) {
                    report.orphan_tool_results += 1;
                    report
                        .drift
                        .push(format!("tool_result {id} has no tool_use"));
                }
            }
            _ => {}
        }
    }
    report
}

/// Repair a resumed history in place so provider tool-call invariants hold.
///
/// Orphan tool *results* (no matching call) are dropped under both policies —
/// there is nothing to anchor them to. Orphan tool *uses* are handled per
/// policy: `Synthesize` appends `[interrupted]` placeholder results right
/// after the call's existing results; `DropOrphan` strips the unanswered
/// calls (and drops the assistant message if nothing remains). Returns a
/// report of what was found and fixed.
pub fn recover_orphaned_tool_results(
    msgs: &mut Vec<ChatMessage>,
    policy: OrphanPolicy,
) -> ConsistencyReport {
    let mut report = ConsistencyReport::default();
    let use_ids = collect_tool_use_ids(msgs);

    // Drop orphan tool results under both policies.
    msgs.retain(|m| {
        if m.role == ChatRole::Tool {
            let id = m.tool_call_id.as_deref().unwrap_or("");
            if !use_ids.contains(id) {
                report.orphan_tool_results += 1;
                report
                    .drift
                    .push(format!("dropped orphan tool_result {id}"));
                return false;
            }
        }
        true
    });

    let mut i = 0;
    while i < msgs.len() {
        let call_ids: Vec<String> = match &msgs[i] {
            m if m.role == ChatRole::Assistant => m
                .tool_calls
                .as_ref()
                .map(|cs| cs.iter().map(|c| c.id.clone()).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if call_ids.is_empty() {
            if msgs[i].role == ChatRole::Assistant
                && msgs[i].content.trim().is_empty()
                && msgs[i].tool_calls.is_none()
            {
                report.empty_assistant += 1;
                report
                    .drift
                    .push("assistant message with neither content nor tool calls".into());
            }
            i += 1;
            continue;
        }

        // Results for this assistant's calls must follow it immediately.
        let mut j = i + 1;
        let mut answered: HashSet<String> = HashSet::new();
        while j < msgs.len() && msgs[j].role == ChatRole::Tool {
            if let Some(id) = &msgs[j].tool_call_id {
                answered.insert(id.clone());
            }
            j += 1;
        }
        let missing: Vec<String> = call_ids
            .iter()
            .filter(|id| !answered.contains(*id))
            .cloned()
            .collect();
        if missing.is_empty() {
            i += 1;
            continue;
        }

        report.orphan_tool_uses += missing.len();
        match policy {
            OrphanPolicy::Synthesize => {
                report.drift.push(format!(
                    "synthesized {} interrupted tool_result(s)",
                    missing.len()
                ));
                for (k, id) in missing.iter().enumerate() {
                    msgs.insert(
                        j + k,
                        ChatMessage {
                            role: ChatRole::Tool,
                            content: INTERRUPTED_PLACEHOLDER.to_string(),
                            name: None,
                            tool_calls: None,
                            tool_call_id: Some(id.clone()),
                            cache_breakpoint: false,
                        },
                    );
                }
                i = j + missing.len();
            }
            OrphanPolicy::DropOrphan => {
                report.drift.push(format!(
                    "dropped {} tool_use(s) without results",
                    missing.len()
                ));
                if let Some(calls) = msgs[i].tool_calls.as_mut() {
                    calls.retain(|c| !missing.contains(&c.id));
                    if calls.is_empty() {
                        msgs[i].tool_calls = None;
                    }
                }
                if msgs[i].content.trim().is_empty() && msgs[i].tool_calls.is_none() {
                    msgs.remove(i);
                    report
                        .drift
                        .push("dropped emptied assistant message".into());
                    continue;
                }
                i += 1;
            }
        }
    }
    report
}

fn collect_tool_use_ids(msgs: &[ChatMessage]) -> HashSet<String> {
    msgs.iter()
        .filter(|m| m.role == ChatRole::Assistant)
        .flat_map(|m| m.tool_calls.as_deref().unwrap_or(&[]))
        .map(|c| c.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_provider::types::{FunctionCall, ToolCall};

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        }
    }

    fn assistant_with_calls(ids: &[&str]) -> ChatMessage {
        let mut msg = ChatMessage::assistant("");
        msg.tool_calls = Some(ids.iter().map(|id| call(id)).collect());
        msg
    }

    fn tool_result(id: &str, content: &str) -> ChatMessage {
        let mut msg = ChatMessage::user(content);
        msg.role = ChatRole::Tool;
        msg.tool_call_id = Some(id.into());
        msg
    }

    #[test]
    fn clean_history_is_untouched() {
        let mut msgs = vec![
            ChatMessage::user("hi"),
            assistant_with_calls(&["c1"]),
            tool_result("c1", "ok"),
            ChatMessage::assistant("done"),
        ];
        let original = msgs.clone();
        let report = recover_orphaned_tool_results(&mut msgs, OrphanPolicy::Synthesize);
        assert!(report.is_clean());
        assert_eq!(msgs, original);
    }

    #[test]
    fn interrupted_parallel_turn_gets_synthesized_result() {
        // Canonical case: parallel batch (A, B) interrupted after A's result.
        let mut msgs = vec![
            ChatMessage::user("do two things"),
            assistant_with_calls(&["a", "b"]),
            tool_result("a", "result a"),
        ];
        let report = recover_orphaned_tool_results(&mut msgs, OrphanPolicy::Synthesize);
        assert_eq!(report.orphan_tool_uses, 1);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3].role, ChatRole::Tool);
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("b"));
        assert_eq!(msgs[3].content, INTERRUPTED_PLACEHOLDER);
    }

    #[test]
    fn drop_orphan_removes_unanswered_calls() {
        let mut msgs = vec![
            ChatMessage::user("do two things"),
            assistant_with_calls(&["a", "b"]),
            tool_result("a", "result a"),
        ];
        let report = recover_orphaned_tool_results(&mut msgs, OrphanPolicy::DropOrphan);
        assert_eq!(report.orphan_tool_uses, 1);
        assert_eq!(msgs.len(), 3);
        let calls = msgs[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "a");
    }

    #[test]
    fn drop_orphan_drops_emptied_assistant_message() {
        let mut msgs = vec![
            ChatMessage::user("hi"),
            assistant_with_calls(&["a"]),
            ChatMessage::user("next"),
        ];
        let report = recover_orphaned_tool_results(&mut msgs, OrphanPolicy::DropOrphan);
        assert_eq!(report.orphan_tool_uses, 1);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].content, "next");
    }

    #[test]
    fn orphan_results_dropped_under_both_policies() {
        for policy in [OrphanPolicy::Synthesize, OrphanPolicy::DropOrphan] {
            let mut msgs = vec![
                ChatMessage::user("hi"),
                tool_result("ghost", "stale"),
                ChatMessage::assistant("hello"),
            ];
            let report = recover_orphaned_tool_results(&mut msgs, policy);
            assert_eq!(report.orphan_tool_results, 1);
            assert_eq!(msgs.len(), 2);
            assert!(report.drift[0].contains("dropped orphan tool_result ghost"));
        }
    }

    #[test]
    fn check_consistency_reports_without_mutating() {
        let msgs = vec![
            ChatMessage::user("hi"),
            assistant_with_calls(&["a", "b"]),
            tool_result("a", "ok"),
            tool_result("ghost", "stale"),
            ChatMessage::assistant(""),
        ];
        let report = check_resume_consistency(&msgs);
        assert_eq!(report.orphan_tool_uses, 1);
        assert_eq!(report.orphan_tool_results, 1);
        assert_eq!(report.empty_assistant, 1);
        assert!(!report.is_clean());
        // Input untouched.
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn check_consistency_clean_for_paired_calls() {
        let msgs = vec![
            ChatMessage::user("hi"),
            assistant_with_calls(&["a"]),
            tool_result("a", "ok"),
        ];
        let report = check_resume_consistency(&msgs);
        assert!(report.is_clean(), "unexpected drift: {:?}", report.drift);
    }
}
