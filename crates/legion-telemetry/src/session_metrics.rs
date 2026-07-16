use serde::{Deserialize, Serialize};

/// Telemetry events emitted by the agent runtime.
///
/// These are intentionally plain data records: they can be written to a local
/// JSONL file, forwarded to an HTTP endpoint, or converted into Prometheus
/// business metrics by downstream consumers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionMetric {
    SessionStarted {
        session_id: String,
        agent_id: String,
        model_ref: String,
    },
    Turn {
        session_id: String,
        turn_number: usize,
        input_tokens: usize,
        model_ref: String,
    },
    TurnCompleted {
        session_id: String,
        turn_number: usize,
        output_tokens: usize,
        tool_calls: usize,
        duration_ms: u64,
    },
    ToolCalled {
        session_id: String,
        turn_number: usize,
        tool: String,
        read_only: bool,
        duration_ms: u64,
    },
    Compaction {
        session_id: String,
        turn_number: usize,
        tokens_before: usize,
        tokens_after: usize,
    },
    DoomLoopRecovery {
        session_id: String,
        turn_number: usize,
        attempts: usize,
        model: String,
    },
}

impl SessionMetric {
    /// Session identifier carried by this event, when present.
    pub fn session_id(&self) -> &str {
        match self {
            SessionMetric::SessionStarted { session_id, .. }
            | SessionMetric::Turn { session_id, .. }
            | SessionMetric::TurnCompleted { session_id, .. }
            | SessionMetric::ToolCalled { session_id, .. }
            | SessionMetric::Compaction { session_id, .. }
            | SessionMetric::DoomLoopRecovery { session_id, .. } => session_id,
        }
    }

    /// Turn number carried by this event, when present.
    pub fn turn_number(&self) -> Option<usize> {
        match self {
            SessionMetric::SessionStarted { .. } => None,
            SessionMetric::Turn { turn_number, .. }
            | SessionMetric::TurnCompleted { turn_number, .. }
            | SessionMetric::ToolCalled { turn_number, .. }
            | SessionMetric::Compaction { turn_number, .. }
            | SessionMetric::DoomLoopRecovery { turn_number, .. } => Some(*turn_number),
        }
    }
}
