//! Inferred commitments (automation-advanced gap Phase B).
//!
//! A background, fire-and-forget extractor scans each finished turn for
//! natural-language follow-ups the user explicitly asked for ("remind me
//! tomorrow to send the report") and schedules one-shot cron jobs for them.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use legion_provider::router::ProviderRouter;
use legion_provider::types::{ChatMessage, ChatRequest, ChatRole};
use legion_runtime::CommitmentExtractor;
use legion_runtime::secret_scanner::SecretScanner;
use serde::Deserialize;

use crate::cron::{CronJob, CronJobStore};

const SYSTEM_PROMPT: &str = "\
You extract commitments from a conversation turn: follow-ups or reminders the \
user explicitly asked the assistant to perform in the future. Return ONLY a \
JSON array of objects with keys \"description\" (what to do, under 200 \
characters) and \"due\" (absolute RFC3339 timestamp in UTC). Use the provided \
current time to resolve relative dates like \"tomorrow\" or \"next Friday\". \
Skip vague suggestions, things already done, and anything the user did not \
explicitly request. If there are no commitments, return []. Example: \
[{\"description\":\"Send the weekly report\",\"due\":\"2026-07-13T09:00:00Z\"}].";

/// Raw commitment shape parsed from the extractor model output.
#[derive(Debug, Deserialize)]
struct RawCommitment {
    description: String,
    due: String,
}

/// A commitment with a validated absolute due time in UTC.
struct Commitment {
    description: String,
    due: DateTime<Utc>,
}

/// Background extractor that turns natural-language follow-ups mentioned in a
/// finished turn into one-shot cron jobs. Constructed only when
/// `commitments.enabled` and a model are configured; all internal failures are
/// logged and swallowed so the main turn is never affected.
pub struct LlmCommitmentExtractor {
    router: Arc<ProviderRouter>,
    model_ref: String,
    store: Arc<dyn CronJobStore>,
    scanner: SecretScanner,
    max_messages: usize,
    max_per_turn: usize,
    cooldown: Arc<Mutex<HashMap<String, Instant>>>,
    cooldown_secs: u64,
    timeout: Duration,
}

impl LlmCommitmentExtractor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: Arc<ProviderRouter>,
        model_ref: impl Into<String>,
        store: Arc<dyn CronJobStore>,
        max_messages: usize,
        max_per_turn: usize,
        cooldown_secs: u64,
        timeout: Duration,
    ) -> Self {
        Self {
            router,
            model_ref: model_ref.into(),
            store,
            scanner: SecretScanner::new(),
            max_messages,
            max_per_turn,
            cooldown: Arc::new(Mutex::new(HashMap::new())),
            cooldown_secs,
            timeout,
        }
    }
}

/// Owned snapshot of the extractor state for a single spawned run. The trait
/// method only borrows `&self`, so each spawn clones the cheap shared handles.
struct Worker {
    router: Arc<ProviderRouter>,
    model_ref: String,
    store: Arc<dyn CronJobStore>,
    scanner: SecretScanner,
    max_messages: usize,
    max_per_turn: usize,
    cooldown: Arc<Mutex<HashMap<String, Instant>>>,
    cooldown_secs: u64,
    timeout: Duration,
}

impl Worker {
    async fn run(&self, agent_id: &str, messages: &[ChatMessage]) {
        if !self.cooldown_allows(agent_id) {
            return;
        }

        let transcript = build_prompt(messages, self.max_messages);
        if transcript.is_empty() {
            return;
        }
        let prompt = format!(
            "{transcript}\nCurrent time (UTC): {}",
            Utc::now().to_rfc3339()
        );

        let req = ChatRequest::new(
            &self.model_ref,
            vec![
                ChatMessage::system(SYSTEM_PROMPT),
                ChatMessage::user(prompt),
            ],
        );

        let Some(text) = legion_runtime::llm::chat_text_with_timeout(
            &self.router,
            &self.model_ref,
            req,
            self.timeout,
        )
        .await
        else {
            return;
        };

        for commitment in parse_commitments(&text, self.max_per_turn) {
            if self.scanner.contains_secret(&commitment.description) {
                tracing::warn!("commitments dropped a candidate containing a secret");
                continue;
            }
            let job = CronJob {
                id: commitment_id(agent_id, &commitment.description, &commitment.due),
                agent_id: agent_id.to_string(),
                message: commitment.description,
                name: String::new(),
                schedule: "__at__".to_string(),
                at: Some(commitment.due),
                enabled: true,
                created_at: Utc::now(),
                next_run: Some(commitment.due),
                last_run: None,
                webhook_secret: None,
            };
            let id = job.id.clone();
            if let Err(e) = self.store.create(job).await {
                tracing::warn!(error = %e, "commitments failed to persist job");
            } else {
                tracing::info!(id = %id, "inferred commitment scheduled");
            }
        }
    }

    /// Check and record the cooldown for `agent_id`. Returns `true` if this run
    /// may proceed (and stamps the cooldown), `false` if it is still cooling down.
    fn cooldown_allows(&self, agent_id: &str) -> bool {
        let now = Instant::now();
        let mut guard = self.cooldown.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = guard.get(agent_id) {
            if now.duration_since(*last) < Duration::from_secs(self.cooldown_secs) {
                return false;
            }
        }
        guard.insert(agent_id.to_string(), now);
        true
    }
}

impl CommitmentExtractor for LlmCommitmentExtractor {
    /// Fire-and-forget extraction for a completed turn. Never blocks the caller.
    fn spawn_extract(&self, agent_id: String, _session_id: String, messages: Vec<ChatMessage>) {
        let worker = Worker {
            router: self.router.clone(),
            model_ref: self.model_ref.clone(),
            store: self.store.clone(),
            scanner: self.scanner,
            max_messages: self.max_messages,
            max_per_turn: self.max_per_turn,
            cooldown: self.cooldown.clone(),
            cooldown_secs: self.cooldown_secs,
            timeout: self.timeout,
        };
        tokio::spawn(async move {
            worker.run(&agent_id, &messages).await;
        });
    }
}

/// Keep the most recent `max_messages` non-system messages and render them as a
/// compact transcript for the extractor prompt.
fn build_prompt(messages: &[ChatMessage], max_messages: usize) -> String {
    let relevant: Vec<&ChatMessage> = messages
        .iter()
        .filter(|m| m.role != ChatRole::System && !m.content.trim().is_empty())
        .collect();
    let start = relevant.len().saturating_sub(max_messages);
    let mut buf = String::new();
    for m in &relevant[start..] {
        let role = match m.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::Tool => "tool",
            ChatRole::System => "system",
        };
        buf.push_str(role);
        buf.push_str(": ");
        buf.push_str(m.content.trim());
        buf.push('\n');
    }
    buf
}

/// Extract the first JSON array of `{description, due}` objects from the model
/// output. Malformed entries, past due times, and empty descriptions are
/// skipped; at most `limit` commitments survive.
fn parse_commitments(text: &str, limit: usize) -> Vec<Commitment> {
    let parsed = legion_runtime::llm::extract_json_array::<RawCommitment>(text).unwrap_or_default();
    let now = Utc::now();
    parsed
        .into_iter()
        .filter_map(|raw| {
            let description = raw.description.trim().to_string();
            if description.is_empty() || description.len() > 200 {
                return None;
            }
            let due = DateTime::parse_from_rfc3339(raw.due.trim())
                .ok()?
                .with_timezone(&Utc);
            if due <= now {
                return None;
            }
            Some(Commitment { description, due })
        })
        .take(limit)
        .collect()
}

fn commitment_id(agent_id: &str, description: &str, due: &DateTime<Utc>) -> String {
    let mut hasher = DefaultHasher::new();
    description.hash(&mut hasher);
    due.to_rfc3339().hash(&mut hasher);
    format!("commitment:{agent_id}:{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_provider::Provider;
    use legion_provider::types::{
        ChatChunk, ChatRequest, ChatStream, EmbedRequest, Embedding, FinishReason, ModelInfo,
        ProviderError,
    };
    use std::sync::Mutex as StdMutex;

    use crate::cron::CronError;

    /// Cron store that records every `create` call.
    #[derive(Default)]
    struct FakeStore {
        created: StdMutex<Vec<CronJob>>,
        /// Signalled on every `create` so tests can await the fire-and-forget
        /// extraction task instead of sleeping.
        notify: tokio::sync::Notify,
    }

    #[async_trait]
    impl CronJobStore for FakeStore {
        async fn create(&self, job: CronJob) -> Result<(), CronError> {
            self.created.lock().unwrap().push(job);
            // `notify_one` stores a permit, so a waiter registered after the
            // create still resolves — no registration race.
            self.notify.notify_one();
            Ok(())
        }
        async fn update(&self, _job: CronJob) -> Result<(), CronError> {
            Ok(())
        }
        async fn remove(&self, _id: &str) -> Result<(), CronError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<CronJob>, CronError> {
            Ok(self.created.lock().unwrap().clone())
        }
        async fn get(&self, id: &str) -> Result<Option<CronJob>, CronError> {
            Ok(self
                .created
                .lock()
                .unwrap()
                .iter()
                .find(|j| j.id == id)
                .cloned())
        }
    }

    /// Provider that always streams the given text.
    struct StaticProvider {
        text: String,
    }

    #[async_trait]
    impl Provider for StaticProvider {
        fn id(&self) -> &str {
            "static"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo::new("gpt", "static")]
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let text = self.text.clone();
            let stream = futures::stream::once(async move {
                Ok(ChatChunk {
                    delta: text,
                    finish_reason: Some(FinishReason::Stop),
                    ..Default::default()
                })
            });
            Ok(Box::pin(stream))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn runtime_extractor(
        text: &str,
        store: Arc<FakeStore>,
        cooldown_secs: u64,
    ) -> LlmCommitmentExtractor {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(StaticProvider {
            text: text.to_string(),
        }));
        LlmCommitmentExtractor::new(
            Arc::new(router),
            "static/gpt",
            store as Arc<dyn CronJobStore>,
            20,
            3,
            cooldown_secs,
            Duration::from_secs(5),
        )
    }

    fn future_due(days: i64) -> String {
        (Utc::now() + chrono::Duration::days(days)).to_rfc3339()
    }

    #[tokio::test]
    async fn schedules_one_shot_cron_job() {
        let store = Arc::new(FakeStore::default());
        let due = future_due(1);
        let ext = runtime_extractor(
            &format!(r#"[{{"description":"Send the weekly report","due":"{due}"}}]"#),
            store.clone(),
            0,
        );
        let messages = vec![
            ChatMessage::user("remind me tomorrow to send the weekly report"),
            ChatMessage::assistant("noted"),
        ];
        ext.spawn_extract("a1".to_string(), "s1".to_string(), messages);
        // Await the actual extraction completion; the timeout only fires if
        // the fire-and-forget task regresses and never persists a job.
        tokio::time::timeout(Duration::from_secs(5), store.notify.notified())
            .await
            .expect("extraction task did not persist a job");

        let jobs = store.created.lock().unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.schedule, "__at__");
        assert_eq!(job.message, "Send the weekly report");
        assert!(job.id.starts_with("commitment:a1:"));
        assert_eq!(
            job.at.map(|t| t.to_rfc3339()),
            Some(DateTime::parse_from_rfc3339(&due).unwrap().to_rfc3339())
        );
        assert!(job.enabled);
        assert_eq!(job.next_run, job.at);
        assert_eq!(job.last_run, None);
    }

    #[tokio::test]
    async fn drops_commitments_containing_secrets() {
        tokio::time::pause();
        let store = Arc::new(FakeStore::default());
        let due = future_due(1);
        let ext = runtime_extractor(
            &format!(
                r#"[{{"description":"api_key=sk-abcdefghijklmnopqrstuvwxyz123456","due":"{due}"}}]"#
            ),
            store.clone(),
            0,
        );
        let messages = vec![ChatMessage::user("remind me"), ChatMessage::assistant("ok")];
        ext.spawn_extract("a1".to_string(), "s1".to_string(), messages);
        // No create may happen: with paused time the runtime fast-forwards to
        // the timeout as soon as the worker task goes idle, so this resolves
        // immediately unless a (regressing) create notifies first.
        let created = tokio::time::timeout(Duration::from_secs(1), store.notify.notified()).await;
        assert!(created.is_err(), "no commitment job should be persisted");
        assert!(store.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skips_past_due_times() {
        tokio::time::pause();
        let store = Arc::new(FakeStore::default());
        let past = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        let ext = runtime_extractor(
            &format!(r#"[{{"description":"Stale reminder","due":"{past}"}}]"#),
            store.clone(),
            0,
        );
        let messages = vec![ChatMessage::user("remind me"), ChatMessage::assistant("ok")];
        ext.spawn_extract("a1".to_string(), "s1".to_string(), messages);
        // Same mechanism as above: paused time fast-forwards to the timeout
        // once the worker is idle; a create would notify before that.
        let created = tokio::time::timeout(Duration::from_secs(1), store.notify.notified()).await;
        assert!(created.is_err(), "no commitment job should be persisted");
        assert!(store.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cooldown_suppresses_repeated_runs() {
        let store = Arc::new(FakeStore::default());
        let due = future_due(1);
        let ext = runtime_extractor(
            &format!(r#"[{{"description":"Ping Bob","due":"{due}"}}]"#),
            store.clone(),
            3600,
        );
        let messages = vec![ChatMessage::user("remind me"), ChatMessage::assistant("ok")];
        ext.spawn_extract("a1".to_string(), "s1".to_string(), messages.clone());
        ext.spawn_extract("a1".to_string(), "s1".to_string(), messages);
        // Exactly one create may happen (cooldown suppresses the second run);
        // await it instead of sleeping.
        tokio::time::timeout(Duration::from_secs(5), store.notify.notified())
            .await
            .expect("extraction task did not persist a job");
        assert_eq!(
            store.created.lock().unwrap().len(),
            1,
            "second run must be suppressed by cooldown"
        );
    }

    #[test]
    fn parse_commitments_handles_wrapped_json() {
        let due = future_due(1);
        let text =
            format!("Sure! Here you go:\n[{{\"description\":\"a\",\"due\":\"{due}\"}}]\nthanks");
        let parsed = parse_commitments(&text, 5);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].description, "a");
    }

    #[test]
    fn parse_commitments_returns_empty_on_garbage() {
        assert!(parse_commitments("no json here", 5).is_empty());
        assert!(parse_commitments("", 5).is_empty());
        assert!(parse_commitments("[]", 5).is_empty());
        // Malformed entry: non-RFC3339 due is skipped.
        assert!(parse_commitments(r#"[{"description":"x","due":"tomorrow"}]"#, 5).is_empty());
    }
}
