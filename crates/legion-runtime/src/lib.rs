pub mod agent_loop;
pub mod approval;
pub mod auto_extract;
pub mod commitments;
pub mod compaction;
pub mod context;
pub mod context_engine;
pub mod coordinator;
pub mod harness;
pub mod memory;
pub mod messenger;
pub mod prompt;
pub mod question;
pub mod recall_selector;
pub mod secret_scanner;
pub mod skill_selector;
pub mod skills_prompt;
pub mod subagent;
pub mod surfaced;
pub mod swarm;
pub mod todo;
pub mod token_counter;
pub mod tool_pipeline;
pub mod tools;
pub mod types;

pub use agent_loop::{AgentRuntime, DEFAULT_MAX_ITERATIONS};
pub use approval::{
    ApprovalGate, ApprovalNotifier, ApprovalQueueRegistry, ApprovalRequest, NoOpApprovalNotifier,
};
pub use auto_extract::AutoExtractor;
pub use commitments::CommitmentExtractor;
pub use context::{
    Filesystem, TokioFs, agent_dir, assemble_system_prompt, resolve_workspace, sessions_dir,
};
pub use context_engine::{ContextEngine, LegacyContextEngine};
pub use coordinator::{
    CoordinatorError, CoordinatorPhase, CoordinatorPlan, CoordinatorReport, CoordinatorTask,
    PhaseReport, run_coordinator_plan,
};
pub use harness::{Harness, HarnessRegistry};
pub use memory::{DecayReport, MemoryBackend, MemoryError, MemoryKind, MemoryNote, RecallContext};
pub use messenger::{AgentMessenger, MessengerError};
pub use prompt::{BuiltPrompt, PromptSection, SectionId, SectionSource, SystemPromptBuilder};
pub use question::{
    AskUserAnnotation, AskUserInput, AskUserOption, AskUserOutput, AskUserQuestion,
    NoOpQuestionNotifier, QuestionCtx, QuestionGate, QuestionNotifier, QuestionQueueRegistry,
    QuestionRequest,
};
pub use recall_selector::LlmRecallSelector;
pub use secret_scanner::SecretScanner;
pub use subagent::{
    RuntimeSubagentSpawner, SubagentError, SubagentHandle, SubagentKind, SubagentRequest,
    SubagentResult, SubagentSpawner, SubagentStatus,
};
pub use surfaced::SurfacedStore;
pub use swarm::{SwarmError, SwarmManager, SwarmMessage, TeammateInfo, TeammateStatus};
pub use todo::{JsonTodoStore, SharedTodoStore, TodoItem, TodoList, TodoStatus, TodoStore};
pub use tools::{Tool, ToolCall, ToolContext, ToolError, ToolRegistry, ToolResult};
pub use types::{LifecyclePhase, RunEvent, RunRequest, RunStream, RuntimeError};

use std::path::PathBuf;

/// Expand a leading `~` in a path using the `HOME` environment variable.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}
