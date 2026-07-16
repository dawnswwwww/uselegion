use legion_provider::types::ChatMessage;

/// Fire-and-forget sink for inferred commitments, invoked at the end of each
/// turn (automation-advanced gap Phase B). Implemented by legion-automation's
/// `LlmCommitmentExtractor`, which turns natural-language follow-ups mentioned
/// in the conversation into one-shot cron jobs. Implementations must log and
/// swallow all failures so the main turn is never affected.
pub trait CommitmentExtractor: Send + Sync {
    fn spawn_extract(&self, agent_id: String, session_id: String, messages: Vec<ChatMessage>);
}
