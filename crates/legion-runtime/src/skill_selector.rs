//! Skill selection strategies for recalling skill bodies.
//!
//! The runtime uses a selector to decide which skill bodies should be injected
//! into the system prompt for a given user intent. By default keyword matching
//! is used; configuring `skills.selector_model` enables a lightweight LLM that
//! picks the most relevant skills from the keyword candidates.

use async_trait::async_trait;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{ChatMessage, ChatRequest};
use legion_skills::Skill;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Default timeout for the LLM skill selector.
const DEFAULT_SELECTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Strategy for selecting skills that should have their bodies injected.
#[async_trait]
pub trait SkillSelector: Send + Sync {
    /// Return the indexes of up to `limit` skills from `candidates` that are
    /// relevant to `intent`.
    async fn select(&self, intent: &str, candidates: &[&Skill], limit: usize) -> Vec<usize>;
}

/// Keyword-based selector. Mirrors the scoring in
/// [`legion_skills::SkillRegistryImpl::relevant`] so the default behaviour is
/// unchanged.
pub struct KeywordSkillSelector;

impl Default for KeywordSkillSelector {
    fn default() -> Self {
        Self
    }
}

impl KeywordSkillSelector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SkillSelector for KeywordSkillSelector {
    async fn select(&self, intent: &str, candidates: &[&Skill], limit: usize) -> Vec<usize> {
        let lowered = intent.to_lowercase();
        let mut scored: Vec<(usize, usize)> = candidates
            .iter()
            .enumerate()
            .map(|(idx, skill)| {
                let mut score = 0;
                let name_lower = skill.frontmatter.name.to_lowercase();
                let desc_lower = skill.frontmatter.description.to_lowercase();
                if name_lower.contains(&lowered) {
                    score += 10;
                }
                if desc_lower.contains(&lowered) {
                    score += 5;
                }
                for word in lowered.split_whitespace() {
                    if name_lower.contains(word) {
                        score += 2;
                    }
                    if desc_lower.contains(word) {
                        score += 1;
                    }
                }
                (idx, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.1));
        scored.into_iter().map(|(idx, _)| idx).take(limit).collect()
    }
}

/// LLM-based selector. Asks a cheap model to pick the most relevant skills from
/// a set of candidates. Failures and timeouts degrade gracefully to an empty
/// selection so the main turn is never blocked.
pub struct LlmSkillSelector {
    router: Arc<ProviderRouter>,
    model_ref: String,
    timeout: Duration,
}

impl LlmSkillSelector {
    pub fn new(router: Arc<ProviderRouter>, model_ref: impl Into<String>) -> Self {
        Self {
            router,
            model_ref: model_ref.into(),
            timeout: DEFAULT_SELECTOR_TIMEOUT,
        }
    }

    /// Override the default 10-second timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl SkillSelector for LlmSkillSelector {
    async fn select(&self, intent: &str, candidates: &[&Skill], limit: usize) -> Vec<usize> {
        if candidates.is_empty() || limit == 0 {
            return Vec::new();
        }

        let system = format!(
            "You are a skill selector. Given a user request and a list of available skills, \
             return a JSON array of up to {} skill names that are most relevant. \
             Return only the JSON array, with no additional commentary.",
            limit
        );

        let mut user = format!("User request: {}\n\nAvailable skills:\n", intent);
        for skill in candidates {
            user.push_str(&format!(
                "- {}: {}\n",
                skill.frontmatter.name, skill.frontmatter.description
            ));
        }
        user.push_str(&format!(
            "\nReturn a JSON array like [\"skill-name\"] with up to {} items.",
            limit
        ));

        let req = ChatRequest::new(
            &self.model_ref,
            vec![ChatMessage::system(system), ChatMessage::user(user)],
        );

        let Some(text) =
            crate::llm::chat_text_with_timeout(&self.router, &self.model_ref, req, self.timeout)
                .await
        else {
            return Vec::new();
        };
        parse_llm_selection(&text, candidates, limit)
    }
}

fn parse_llm_selection(text: &str, candidates: &[&Skill], limit: usize) -> Vec<usize> {
    let Some(names) = crate::llm::extract_json_array::<String>(text) else {
        tracing::warn!(text = %text, "skill selector response did not contain a JSON array");
        return Vec::new();
    };

    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        if let Some((idx, _)) = candidates
            .iter()
            .enumerate()
            .find(|(_, s)| s.frontmatter.name == name)
        {
            if seen.insert(idx) {
                selected.push(idx);
            }
        }
    }
    selected.into_iter().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_provider::provider::Provider;
    use legion_provider::router::ProviderRouter;
    use legion_provider::types::{
        ChatChunk, ChatRequest, ChatStream, EmbedRequest, Embedding, FinishReason, ModelInfo,
        ProviderError,
    };
    use legion_skills::{Skill, SkillFrontmatter, SkillSource};
    use std::path::PathBuf;

    fn make_skill(name: &str, description: &str) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: name.to_string(),
                description: description.to_string(),
                when_to_use: None,
                allowed_tools: Vec::new(),
                paths: Vec::new(),
                user_invocable: true,
                model: None,
                effort: None,
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        }
    }

    #[tokio::test]
    async fn keyword_selector_matches_name_and_description() {
        let rust = make_skill("rust", "Help with Rust code");
        let python = make_skill("python", "Help with Python code");
        let candidates = vec![&rust, &python];

        let selector = KeywordSkillSelector::new();
        let selected = selector.select("write rust", &candidates, 2).await;

        assert_eq!(selected, vec![0]);
    }

    #[tokio::test]
    async fn keyword_selector_respects_limit() {
        let a = make_skill("a", "first");
        let b = make_skill("b", "second");
        let c = make_skill("c", "third");
        let candidates = vec![&a, &b, &c];

        let selector = KeywordSkillSelector::new();
        let selected = selector.select("first second third", &candidates, 2).await;

        assert_eq!(selected.len(), 2);
    }

    #[tokio::test]
    async fn keyword_selector_returns_empty_for_no_match() {
        let rust = make_skill("rust", "Help with Rust code");
        let candidates = vec![&rust];

        let selector = KeywordSkillSelector::new();
        let selected = selector.select("terraform", &candidates, 2).await;

        assert!(selected.is_empty());
    }

    #[test]
    fn parse_llm_selection_extracts_named_skills() {
        let rust = make_skill("rust", "Rust help");
        let python = make_skill("python", "Python help");
        let candidates = vec![&rust, &python];

        let selected = parse_llm_selection(r#"["python"]"#, &candidates, 2);
        assert_eq!(selected, vec![1]);
    }

    #[test]
    fn parse_llm_selection_limits_results() {
        let rust = make_skill("rust", "Rust help");
        let python = make_skill("python", "Python help");
        let go = make_skill("go", "Go help");
        let candidates = vec![&rust, &python, &go];

        let selected = parse_llm_selection(r#"["rust", "python", "go"]"#, &candidates, 2);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn parse_llm_selection_ignores_unknown_names() {
        let rust = make_skill("rust", "Rust help");
        let candidates = vec![&rust];

        let selected = parse_llm_selection(r#"["rust", "unknown"]"#, &candidates, 2);
        assert_eq!(selected, vec![0]);
    }

    #[test]
    fn parse_llm_selection_returns_empty_for_invalid_json() {
        let rust = make_skill("rust", "Rust help");
        let candidates = vec![&rust];

        let selected = parse_llm_selection("not json", &candidates, 2);
        assert!(selected.is_empty());
    }

    struct StaticProvider {
        response: String,
    }

    #[async_trait]
    impl Provider for StaticProvider {
        fn id(&self) -> &str {
            "static"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let chunk = ChatChunk {
                index: 0,
                delta: self.response.clone(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn llm_selector_parses_provider_response() {
        let rust = make_skill("rust", "Rust help");
        let python = make_skill("python", "Python help");
        let candidates = vec![&rust, &python];

        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(StaticProvider {
            response: r#"["python"]"#.to_string(),
        }));

        let selector = LlmSkillSelector::new(Arc::new(router), "static/gpt");
        let selected = selector.select("help with python", &candidates, 2).await;

        assert_eq!(selected, vec![1]);
    }

    #[tokio::test]
    async fn llm_selector_returns_empty_on_provider_error() {
        struct FailingProvider;

        #[async_trait]
        impl Provider for FailingProvider {
            fn id(&self) -> &str {
                "failing"
            }

            fn supported_models(&self) -> Vec<ModelInfo> {
                Vec::new()
            }

            async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
                Err(ProviderError::AllProvidersFailed)
            }

            async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
                Ok(Vec::new())
            }
        }

        let rust = make_skill("rust", "Rust help");
        let candidates = vec![&rust];

        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(FailingProvider));

        let selector = LlmSkillSelector::new(Arc::new(router), "failing/gpt");
        let selected = selector.select("help", &candidates, 2).await;

        assert!(selected.is_empty());
    }

    #[tokio::test]
    async fn llm_selector_respects_timeout() {
        struct SlowProvider;

        #[async_trait]
        impl Provider for SlowProvider {
            fn id(&self) -> &str {
                "slow"
            }

            fn supported_models(&self) -> Vec<ModelInfo> {
                Vec::new()
            }

            async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let chunk = ChatChunk {
                    index: 0,
                    delta: "[]".to_string(),
                    finish_reason: Some(FinishReason::Stop),
                    tool_calls: None,
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            }

            async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
                Ok(Vec::new())
            }
        }

        let rust = make_skill("rust", "Rust help");
        let candidates = vec![&rust];

        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(SlowProvider));

        let selector = LlmSkillSelector::new(Arc::new(router), "slow/gpt")
            .with_timeout(Duration::from_millis(10));
        let selected = selector.select("help", &candidates, 2).await;

        assert!(selected.is_empty());
    }
}
