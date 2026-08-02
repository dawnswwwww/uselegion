use std::sync::Arc;
use std::time::Duration;

use legion_provider::router::ProviderRouter;
use legion_provider::types::{ChatMessage, ChatRequest};

use crate::memory::MemoryNote;

const SYSTEM_PROMPT: &str = "\
You select the memories most relevant to a user query. Given a numbered list of \
candidate memories and a query, return ONLY a JSON array of the 0-based indices of \
the most relevant candidates, ordered by relevance (most relevant first). Omit \
irrelevant ones. Example: [2, 0, 3]. Return [] if none are relevant.";

/// Optional LLM re-ranker for recalled memories (Phase C). Mirrors
/// [`crate::skill_selector::LlmSkillSelector`]: ask a cheap model to pick the most
/// relevant candidate indices, then reorder accordingly. All failures fall back to
/// the original ranking so recall never hard-fails.
pub struct LlmRecallSelector {
    router: Arc<ProviderRouter>,
    model_ref: String,
    timeout: Duration,
}

impl LlmRecallSelector {
    pub fn new(
        router: Arc<ProviderRouter>,
        model_ref: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            router,
            model_ref: model_ref.into(),
            timeout,
        }
    }

    /// Reorder `candidates` by relevance to `query`, returning at most `limit`.
    /// On any LLM/parse error, falls back to the original order truncated to `limit`.
    pub async fn select(
        &self,
        query: &str,
        candidates: Vec<MemoryNote>,
        limit: usize,
    ) -> Vec<MemoryNote> {
        if candidates.is_empty() || limit == 0 {
            return Vec::new();
        }

        let mut user = format!("Query: {query}\n\nCandidates:\n");
        for (i, n) in candidates.iter().enumerate() {
            let snippet: String = n.content.chars().take(200).collect();
            let id = &n.id;
            user.push_str(&format!("{i}. [{id}] {snippet}\n"));
        }
        user.push_str(&format!(
            "\nReturn a JSON array of up to {limit} indices, most relevant first."
        ));

        let req = ChatRequest::new(
            &self.model_ref,
            vec![ChatMessage::system(SYSTEM_PROMPT), ChatMessage::user(user)],
        );

        let text = match crate::llm::chat_text_with_timeout(
            &self.router,
            &self.model_ref,
            req,
            self.timeout,
        )
        .await
        {
            Some(text) => text,
            None => return fallback(candidates, limit),
        };

        let indices = parse_indices(&text);
        if indices.is_empty() {
            return fallback(candidates, limit);
        }

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(limit);
        for idx in indices {
            if !seen.insert(idx) {
                continue;
            }
            if idx < candidates.len() {
                out.push(candidates[idx].clone());
                if out.len() >= limit {
                    break;
                }
            }
        }
        if out.is_empty() {
            return fallback(candidates, limit);
        }
        out
    }
}

fn fallback(candidates: Vec<MemoryNote>, limit: usize) -> Vec<MemoryNote> {
    candidates.into_iter().take(limit).collect()
}

/// Extract the first JSON array of non-negative integers from the model output.
fn parse_indices(text: &str) -> Vec<usize> {
    crate::llm::extract_json_array(text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_provider::types::{
        ChatChunk, ChatRequest, ChatStream, EmbedRequest, Embedding, FinishReason, ModelInfo,
        ProviderError,
    };

    struct StaticProvider {
        text: String,
    }

    #[async_trait]
    impl legion_provider::Provider for StaticProvider {
        fn id(&self) -> &str {
            "static"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let text = self.text.clone();
            Ok(Box::pin(futures::stream::once(async move {
                Ok(ChatChunk {
                    delta: text,
                    finish_reason: Some(FinishReason::Stop),
                    ..Default::default()
                })
            })))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn selector(text: &str) -> LlmRecallSelector {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(StaticProvider {
            text: text.to_string(),
        }));
        LlmRecallSelector::new(Arc::new(router), "static/gpt", Duration::from_secs(5))
    }

    fn note(id: &str) -> MemoryNote {
        MemoryNote {
            id: id.to_string(),
            content: id.to_string(),
            score: 1.0,
            kind: None,
        }
    }

    #[tokio::test]
    async fn reorders_by_llm_indices() {
        let sel = selector("[2, 0]");
        let notes = sel
            .select("q", vec![note("a"), note("b"), note("c")], 3)
            .await;
        let ids: Vec<_> = notes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a"]);
    }

    #[tokio::test]
    async fn falls_back_on_garbage() {
        let sel = selector("not json");
        let notes = sel.select("q", vec![note("a"), note("b")], 1).await;
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, "a");
    }

    #[tokio::test]
    async fn respects_limit_and_dedups_indices() {
        let sel = selector("[1, 1, 0, 2]");
        let notes = sel
            .select("q", vec![note("a"), note("b"), note("c")], 2)
            .await;
        let ids: Vec<_> = notes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn parse_indices_extracts_first_array() {
        assert_eq!(parse_indices("here [3, 1, 2] done"), vec![3, 1, 2]);
        assert!(parse_indices("nothing").is_empty());
    }
}
