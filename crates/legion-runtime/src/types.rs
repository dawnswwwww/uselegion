use futures::Stream;
use serde::Serialize;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

use crate::approval::ApprovalGate;
use crate::memory::MemoryError;
use crate::question::QuestionGate;
use crate::tools::{ToolCall, ToolError, ToolResult};
use legion_provider::types::ProviderError;

/// Errors that can occur during an agent run.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    #[error("tool error: {0}")]
    Tool(#[from] ToolError),

    #[error("memory error: {0}")]
    Memory(#[from] MemoryError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("context assembly failed: {0}")]
    Context(String),

    #[error("max iterations ({0}) exceeded without a final answer")]
    MaxIterations(usize),
}

/// A boxed, sendable stream of runtime events.
pub type RunStream = Pin<Box<dyn Stream<Item = RunEvent> + Send>>;

/// Lifecycle phases emitted by the runtime.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    Start,
    End,
    Error,
}

/// Events produced by an agent run.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum RunEvent {
    Lifecycle {
        phase: LifecyclePhase,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    AssistantDelta {
        delta: String,
    },
    ToolStart {
        tool_call: ToolCall,
    },
    ToolEnd {
        tool_call: ToolCall,
        result: ToolResult,
    },
    Compaction {
        summary: String,
        boundary: Option<BoundaryMark>,
        /// The effective post-compaction history (excluding the leading system
        /// prompt, which is rebuilt on resume). The gateway persists it right
        /// after the boundary marker so `load_for_resume` can reconstruct the
        /// compacted context from the transcript tail (session-resume Phase A).
        resume_head: Vec<legion_provider::types::ChatMessage>,
    },
    /// The model has updated the session todo list via the `todo_write` tool.
    /// Consumers (TUI, gateway clients) should refresh their displayed checklist.
    TodoUpdate {
        list: crate::todo::TodoList,
    },
}

/// Persistent marker written to the session transcript when compaction occurs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoundaryMark {
    /// Index of this boundary within the transcript (0-based line count before
    /// the boundary was written).
    pub entry_index: usize,
    /// ISO-8601 timestamp of when the compaction occurred.
    pub timestamp_iso: String,
    /// Estimated number of tokens that were compacted.
    pub tokens_compacted: usize,
}

/// Reattachments injected after compaction so the model "wakes up" with its
/// capabilities intact.
#[derive(Debug, Clone, PartialEq)]
pub enum Reattachment {
    /// Files the agent has read in this session.
    ViewedFiles(Vec<String>),
    /// Configured skill names active for this agent.
    ActiveSkills(Vec<String>),
    /// Memory notes recalled for the current task.
    RecalledMemory(Vec<String>),
    /// Tool definitions available to the agent.
    ToolManifest(Vec<legion_provider::types::ToolDefinition>),
}

impl Reattachment {
    /// Render the reattachment as one or more system messages.
    pub fn to_messages(&self) -> Vec<legion_provider::types::ChatMessage> {
        match self {
            Reattachment::ViewedFiles(files) => {
                let list = files
                    .iter()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                vec![legion_provider::types::ChatMessage::system(format!(
                    "Files you have viewed in this session:\n{list}"
                ))]
            }
            Reattachment::ActiveSkills(skills) => {
                let list = skills
                    .iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                vec![legion_provider::types::ChatMessage::system(format!(
                    "Active skills for this session:\n{list}"
                ))]
            }
            Reattachment::RecalledMemory(notes) => {
                let list = notes
                    .iter()
                    .map(|n| format!("- {n}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                vec![legion_provider::types::ChatMessage::system(format!(
                    "Recalled relevant memory:\n{list}"
                ))]
            }
            Reattachment::ToolManifest(defs) => {
                let manifest = serde_json::json!({
                    "tools": defs.iter().map(|d| {
                        serde_json::json!({
                            "name": d.name,
                            "description": d.description,
                            "parameters": d.parameters,
                        })
                    }).collect::<Vec<_>>()
                });
                let rendered = serde_json::to_string_pretty(&manifest).unwrap_or_default();
                vec![legion_provider::types::ChatMessage::system(format!(
                    "Available tools:\n{rendered}"
                ))]
            }
        }
    }
}

/// Result of compacting a conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    /// Generated summary of the summarized message window.
    pub summary: String,
    /// Replacement message list: original system message, summary message, and
    /// the kept recent messages.
    pub messages: Vec<legion_provider::types::ChatMessage>,
    /// Estimated tokens before compaction.
    pub tokens_before: usize,
    /// Estimated tokens after compaction.
    pub tokens_after: usize,
    /// Whether any messages were actually summarized.
    pub compacted: bool,
    /// State reattachments injected after compaction.
    pub reattachments: Vec<Reattachment>,
    /// Boundary marker persisted to the transcript for session-resume.
    pub boundary: Option<BoundaryMark>,
}

/// Request to start an agent run.
#[derive(Clone)]
pub struct RunRequest {
    pub session_id: String,
    pub agent_id: String,
    pub user_message: String,
    pub model_ref: String,
    pub system_prompt: Option<String>,
    pub history: Vec<legion_provider::types::ChatMessage>,
    /// Whether the run is interactive (a human is present to answer approval
    /// prompts). Unattended runs (cron, heartbeat) should set this to `false`.
    pub interactive: bool,
    /// Optional sender identifier (e.g. `"tg:123"`) used for `allow_from`
    /// policy checks and approval routing.
    pub sender: Option<String>,
    /// Optional pre-constructed approval gate. When `None`, the runtime creates
    /// a gate with a no-op notifier (prompts time out and are denied).
    pub approval_gate: Option<Arc<ApprovalGate>>,
    /// Optional pre-constructed question gate. When `None`, the runtime creates
    /// a gate with a no-op notifier (questions time out and are denied).
    pub question_gate: Option<Arc<QuestionGate>>,
    /// Nesting depth of this run (0 for top-level). Sub-agent runs increment
    /// this; spawns at/above `subagents.max_depth` are rejected.
    pub depth: u8,
    /// When `Some`, restricts this run to the named tool subset (permission
    /// narrowing for sub-agents). `None` means the full registry is available.
    pub allowed_tools: Option<Vec<String>>,
    /// Per-run override for the tool-iteration cap. `None` falls back to the
    /// runtime's configured `max_iterations`.
    pub max_iterations: Option<usize>,
    /// Whether to dump the assembled system prompt for this run to
    /// `~/.legion/dump-prompts/<session>.jsonl` (also enabled globally via
    /// `promptDump.enabled`).
    pub dump_prompts: bool,
    /// Per-run workspace override. When `Some`, the agent works in this
    /// directory instead of the config-resolved workspace — this drives
    /// tool (read/write/exec) paths, bootstrap files, skills, and the system
    /// prompt. Only embedded/local CLI runs set this (e.g. `--workspace` /
    /// cwd default); the gateway and channel paths always leave it `None`
    /// and resolve from config. The memory backend is unaffected (always
    /// `~/.legion`).
    pub workspace_override: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunRequest")
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("user_message", &self.user_message)
            .field("model_ref", &self.model_ref)
            .field("system_prompt", &self.system_prompt)
            .field("history", &self.history)
            .field("interactive", &self.interactive)
            .field("sender", &self.sender)
            .field("approval_gate", &self.approval_gate.is_some())
            .field("question_gate", &self.question_gate.is_some())
            .field("depth", &self.depth)
            .field("allowed_tools", &self.allowed_tools)
            .field("max_iterations", &self.max_iterations)
            .field("dump_prompts", &self.dump_prompts)
            .field("workspace_override", &self.workspace_override)
            .finish()
    }
}

impl RunRequest {
    pub fn new(
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
        user_message: impl Into<String>,
        model_ref: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            user_message: user_message.into(),
            model_ref: model_ref.into(),
            system_prompt: None,
            history: Vec::new(),
            interactive: true,
            sender: None,
            approval_gate: None,
            question_gate: None,
            depth: 0,
            allowed_tools: None,
            max_iterations: None,
            dump_prompts: false,
            workspace_override: None,
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn with_history(mut self, history: Vec<legion_provider::types::ChatMessage>) -> Self {
        self.history = history;
        self
    }

    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    pub fn with_sender(mut self, sender: impl Into<String>) -> Self {
        self.sender = Some(sender.into());
        self
    }

    pub fn with_approval_gate(mut self, gate: Arc<ApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self
    }

    pub fn with_question_gate(mut self, gate: Arc<QuestionGate>) -> Self {
        self.question_gate = Some(gate);
        self
    }

    pub fn with_depth(mut self, depth: u8) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_allowed_tools(mut self, allowed_tools: Vec<String>) -> Self {
        self.allowed_tools = Some(allowed_tools);
        self
    }

    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = Some(max_iterations);
        self
    }

    pub fn with_dump_prompts(mut self, dump_prompts: bool) -> Self {
        self.dump_prompts = dump_prompts;
        self
    }

    pub fn with_workspace_override(mut self, workspace: Option<std::path::PathBuf>) -> Self {
        self.workspace_override = workspace;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_provider::types::{ChatRole, ToolDefinition};

    fn single_system_content(msgs: &[legion_provider::types::ChatMessage]) -> &str {
        assert_eq!(msgs.len(), 1, "reattachment must render one message");
        assert_eq!(msgs[0].role, ChatRole::System);
        &msgs[0].content
    }

    #[test]
    fn reattachment_renders_expected_system_messages() {
        let msgs =
            Reattachment::ViewedFiles(vec!["src/main.rs".into(), "README.md".into()]).to_messages();
        let content = single_system_content(&msgs);
        assert!(content.starts_with("Files you have viewed in this session:\n"));
        assert!(content.contains("- src/main.rs\n- README.md"));

        let msgs = Reattachment::ActiveSkills(vec!["deploy".into(), "review".into()]).to_messages();
        let content = single_system_content(&msgs);
        assert!(content.starts_with("Active skills for this session:\n"));
        assert!(content.contains("- deploy\n- review"));

        let msgs =
            Reattachment::RecalledMemory(vec!["user prefers dark mode".into()]).to_messages();
        let content = single_system_content(&msgs);
        assert!(content.starts_with("Recalled relevant memory:\n"));
        assert!(content.contains("- user prefers dark mode"));

        let msgs = Reattachment::ToolManifest(vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }])
        .to_messages();
        let content = single_system_content(&msgs);
        assert!(content.starts_with("Available tools:\n"));
        let json = &content["Available tools:\n".len()..];
        let parsed: serde_json::Value =
            serde_json::from_str(json).expect("tool manifest must render valid JSON");
        assert_eq!(parsed["tools"][0]["name"], "read");
        assert_eq!(parsed["tools"][0]["description"], "Read a file");
        assert_eq!(
            parsed["tools"][0]["parameters"],
            serde_json::json!({"type": "object"})
        );
    }
}
