use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{ChatMessage, ChatRequest, ChatRole};

use crate::memory::{MemoryBackend, MemoryMeta};
use crate::secret_scanner::SecretScanner;

const SYSTEM_PROMPT: &str = "\
You extract durable, long-term facts from a conversation turn for an agent's \
memory. Return ONLY a JSON array of short strings, each a single fact worth \
remembering (user preferences, project decisions, recurring constraints). Skip \
ephemeral chatter, greetings, and anything already obvious. If nothing is worth \
remembering, return []. Keep each fact under 200 characters. Example: \
[\"User prefers dark mode\",\"Project deploys to staging on Fridays\"].";

/// Background extractor that distils durable facts from a finished turn into the
/// Episodic memory layer. Constructed only when `memory.autoExtract.enabled` and
/// a model are configured; all internal failures are logged and swallowed so the
/// main turn is never affected.
pub struct AutoExtractor {
    router: Arc<ProviderRouter>,
    model_ref: String,
    memory: Arc<dyn MemoryBackend>,
    scanner: SecretScanner,
    max_messages: usize,
    max_facts_per_turn: usize,
    cooldown: Mutex<HashMap<String, Instant>>,
    cooldown_secs: u64,
    timeout: Duration,
}

impl AutoExtractor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: Arc<ProviderRouter>,
        model_ref: impl Into<String>,
        memory: Arc<dyn MemoryBackend>,
        max_messages: usize,
        max_facts_per_turn: usize,
        cooldown_secs: u64,
        timeout: Duration,
    ) -> Self {
        Self {
            router,
            model_ref: model_ref.into(),
            memory,
            scanner: SecretScanner::new(),
            max_messages,
            max_facts_per_turn,
            cooldown: Mutex::new(HashMap::new()),
            cooldown_secs,
            timeout,
        }
    }

    /// Fire-and-forget extraction for a completed turn. Never blocks the caller.
    pub fn spawn(
        self: Arc<Self>,
        agent_id: String,
        session_id: String,
        messages: Vec<ChatMessage>,
    ) {
        tokio::spawn(async move {
            self.run(&agent_id, &session_id, &messages).await;
        });
    }

    async fn run(&self, agent_id: &str, session_id: &str, messages: &[ChatMessage]) {
        if !self.cooldown_allows(agent_id) {
            return;
        }

        let prompt = build_prompt(messages, self.max_messages);
        if prompt.is_empty() {
            return;
        }

        let req = ChatRequest::new(
            &self.model_ref,
            vec![
                ChatMessage::system(SYSTEM_PROMPT),
                ChatMessage::user(prompt),
            ],
        );

        let text = match tokio::time::timeout(self.timeout, self.router.chat(&self.model_ref, req))
            .await
        {
            Ok(Ok(mut stream)) => {
                let mut buf = String::new();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(c) => buf.push_str(&c.delta),
                        Err(e) => {
                            tracing::warn!(error = %e, "auto_extract stream error");
                            return;
                        }
                    }
                }
                buf
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "auto_extract LLM call failed");
                return;
            }
            Err(_) => {
                tracing::warn!("auto_extract LLM call timed out");
                return;
            }
        };

        let facts = parse_facts(&text, self.max_facts_per_turn);
        for fact in facts {
            if self.scanner.contains_secret(&fact) {
                tracing::warn!("auto_extract dropped a candidate fact containing a secret");
                continue;
            }
            let id = fact_id(agent_id, &fact);
            let meta = MemoryMeta {
                source: Some(session_id.to_string()),
                kind: Some("episodic".to_string()),
                ..Default::default()
            };
            if let Err(e) = self.memory.index(&id, &fact, meta).await {
                tracing::warn!(error = %e, "auto_extract failed to index fact");
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

/// Extract the first JSON array of strings from the model output.
fn parse_facts(text: &str, limit: usize) -> Vec<String> {
    let start = text.find('[');
    let end = text.rfind(']');
    let (Some(s), Some(e)) = (start, end) else {
        return Vec::new();
    };
    if e < s {
        return Vec::new();
    }
    let slice = &text[s..=e];
    let parsed: Vec<String> = serde_json::from_str(slice).unwrap_or_default();
    parsed
        .into_iter()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .take(limit)
        .collect()
}

fn fact_id(agent_id: &str, fact: &str) -> String {
    let mut hasher = DefaultHasher::new();
    fact.hash(&mut hasher);
    format!("auto:{agent_id}:{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_provider::types::{
        ChatChunk, ChatRequest, ChatStream, EmbedRequest, Embedding, FinishReason, ModelInfo,
        ProviderError,
    };
    use std::ops::Range;
    use std::sync::Mutex as StdMutex;

    use crate::memory::{MemoryError, MemoryNote};

    #[derive(Default)]
    struct FakeMemory {
        indexed: StdMutex<Vec<(String, String, MemoryMeta)>>,
    }

    #[async_trait]
    impl MemoryBackend for FakeMemory {
        async fn search(&self, _q: &str, _k: usize) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(Vec::new())
        }
        async fn get(&self, _p: &str, _r: Option<Range<usize>>) -> Result<String, MemoryError> {
            Ok(String::new())
        }
        async fn index(
            &self,
            id: &str,
            content: &str,
            meta: MemoryMeta,
        ) -> Result<(), MemoryError> {
            self.indexed
                .lock()
                .unwrap()
                .push((id.to_string(), content.to_string(), meta));
            Ok(())
        }
    }

    /// Provider that always streams the given text.
    struct StaticProvider {
        text: String,
    }

    #[async_trait]
    impl legion_provider::Provider for StaticProvider {
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
        memory: Arc<FakeMemory>,
        cooldown_secs: u64,
    ) -> Arc<AutoExtractor> {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(StaticProvider {
            text: text.to_string(),
        }));
        Arc::new(AutoExtractor::new(
            Arc::new(router),
            "static/gpt",
            memory as Arc<dyn MemoryBackend>,
            20,
            5,
            cooldown_secs,
            Duration::from_secs(5),
        ))
    }

    #[tokio::test]
    async fn indexes_clean_facts_and_drops_secrets() {
        let memory = Arc::new(FakeMemory::default());
        let ext = runtime_extractor(
            r#"["User prefers Rust","api_key=sk-abcdefghijklmnopqrstuvwxyz123456"]"#,
            memory.clone(),
            0,
        );
        let messages = vec![
            ChatMessage::user("what stack do we use?"),
            ChatMessage::assistant("mostly Rust"),
        ];
        ext.clone()
            .spawn("a1".to_string(), "s1".to_string(), messages);
        // Yield so the spawned task runs.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let indexed = memory.indexed.lock().unwrap();
        assert_eq!(indexed.len(), 1, "secret fact must be dropped");
        assert_eq!(indexed[0].1, "User prefers Rust");
        assert_eq!(indexed[0].2.kind, Some("episodic".to_string()));
    }

    #[tokio::test]
    async fn cooldown_suppresses_repeated_runs() {
        let memory = Arc::new(FakeMemory::default());
        let ext = runtime_extractor(r#"["fact one"]"#, memory.clone(), 3600);
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("there")];
        ext.clone()
            .spawn("a1".to_string(), "s1".to_string(), messages.clone());
        ext.clone()
            .spawn("a1".to_string(), "s1".to_string(), messages);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let indexed = memory.indexed.lock().unwrap();
        assert_eq!(
            indexed.len(),
            1,
            "second run must be suppressed by cooldown"
        );
    }

    #[tokio::test]
    async fn parse_facts_handles_wrapped_json() {
        let facts = parse_facts("Sure! Here you go:\n[\"a\",\"b\"]\nthanks", 5);
        assert_eq!(facts, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn parse_facts_returns_empty_on_garbage() {
        assert!(parse_facts("no json here", 5).is_empty());
        assert!(parse_facts("", 5).is_empty());
    }
}
