use crate::auth::AuthProfile;
use crate::http;
use crate::model_ref::{parse_model_ref, resolve_model_ref};
use crate::openai::OpenAiProvider;
use crate::ops::{
    CostTracker, RateLimiter, RetryPolicy, estimate_chat_tokens, estimate_tokens, is_retryable,
    track_chat_cost,
};
use crate::provider::Provider;
use crate::types::{
    ChatRequest, ChatStream, EmbedRequest, Embedding, ImageRequest, ImageResponse, ProviderError,
    ResolvedModelRef, SpeechRequest, SpeechResponse, VideoRequest, VideoResponse,
};
use legion_core::config::{ModelCost, ProviderConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Routes model references to concrete providers and applies fallback chains.
///
/// Each candidate provider call is wrapped with per-provider rate limiting,
/// a single-provider retry loop (429/5xx/timeout are retryable), and the
/// configured `timeout_seconds`; successful chat streams are cost-tracked.
pub struct ProviderRouter {
    providers: HashMap<String, Arc<dyn Provider>>,
    aliases: HashMap<String, String>,
    fallbacks: Vec<String>,
    retry: HashMap<String, RetryPolicy>,
    rate_limiter: RateLimiter,
    timeouts: HashMap<String, Duration>,
    cost: Arc<CostTracker>,
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRouter {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            aliases: HashMap::new(),
            fallbacks: Vec::new(),
            retry: HashMap::new(),
            rate_limiter: RateLimiter::new(),
            timeouts: HashMap::new(),
            cost: Arc::new(CostTracker::new(HashMap::new(), None)),
        }
    }

    /// Register a provider implementation.
    pub fn register_provider(&mut self, provider: Arc<dyn Provider>) {
        self.providers.insert(provider.id().to_string(), provider);
    }

    /// Set the alias table.
    pub fn set_aliases(&mut self, aliases: HashMap<String, String>) {
        self.aliases = aliases;
    }

    /// Set the global fallback chain.
    pub fn set_fallbacks(&mut self, fallbacks: Vec<String>) {
        self.fallbacks = fallbacks;
    }

    /// Access the cost tracker (e.g. for metrics or snapshots).
    pub fn cost_tracker(&self) -> Arc<CostTracker> {
        self.cost.clone()
    }

    /// Build a router from `ProviderConfig` entries plus their auth profiles.
    ///
    /// `costs` supplies the per-model cost rates and `costs_path` an optional
    /// JSON file the cost tracker write-through persists to. Per-provider
    /// `retry` / `rateLimit` / `timeoutSeconds` config is wired up here.
    pub fn from_configs(
        configs: &HashMap<String, ProviderConfig>,
        auth_profiles: &HashMap<String, AuthProfile>,
        costs: &HashMap<String, ModelCost>,
        costs_path: Option<PathBuf>,
    ) -> Result<Self, ProviderError> {
        let mut router = Self::new();
        router.cost = Arc::new(CostTracker::new(costs.clone(), costs_path));

        for config in configs.values() {
            let auth = auth_profiles
                .get(&config.auth_profile)
                .cloned()
                .unwrap_or_else(|| AuthProfile::api_key(""));

            let provider: Arc<dyn Provider> = match config.kind.as_str() {
                "openai" | "generic-openai" | "openrouter" => {
                    let mut p = OpenAiProvider::new(&config.id, config.base_url.clone(), auth)?;
                    if let Some(model) = &config.default_model {
                        p = p.with_model(model);
                    }
                    Arc::new(p)
                }
                "anthropic" => {
                    let mut p = crate::anthropic::AnthropicProvider::new(
                        &config.id,
                        config.base_url.clone(),
                        auth,
                    )?;
                    if let Some(model) = &config.default_model {
                        p = p.with_model(model);
                    }
                    Arc::new(p)
                }
                "gemini" => {
                    let mut p = crate::gemini::GeminiProvider::new(
                        &config.id,
                        config.base_url.clone(),
                        auth,
                    )?;
                    if let Some(model) = &config.default_model {
                        p = p.with_model(model);
                    }
                    Arc::new(p)
                }
                "ollama" => {
                    let mut p = crate::ollama::OllamaProvider::new(
                        &config.id,
                        config.base_url.clone(),
                        auth,
                    )?;
                    if let Some(model) = &config.default_model {
                        p = p.with_model(model);
                    }
                    Arc::new(p)
                }
                "bedrock" => {
                    let mut p = crate::bedrock::BedrockProvider::new(
                        &config.id,
                        config.base_url.clone(),
                        auth,
                    )?;
                    if let Some(model) = &config.default_model {
                        p = p.with_model(model);
                    }
                    Arc::new(p)
                }
                other => {
                    return Err(ProviderError::ProviderNotFound(format!(
                        "unsupported provider kind '{other}'"
                    )));
                }
            };

            if let Some(retry) = &config.retry {
                router
                    .retry
                    .insert(config.id.clone(), RetryPolicy::from_config(retry));
            }
            if let Some(rate_limit) = &config.rate_limit {
                router.rate_limiter.configure(&config.id, rate_limit);
            }
            if let Some(seconds) = config.timeout_seconds {
                router
                    .timeouts
                    .insert(config.id.clone(), Duration::from_secs(seconds));
            }

            router.register_provider(provider);
        }

        Ok(router)
    }

    /// Run one candidate's call under the provider's retry policy and
    /// configured timeout, logging each attempt. Returns the final error
    /// when retries are exhausted or the error is non-retryable.
    async fn call_with_retry<T, F, Fut>(
        &self,
        candidate: &ResolvedModelRef,
        model: &str,
        success_message: &'static str,
        mut call: F,
    ) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, ProviderError>>,
    {
        let policy = self.retry.get(&candidate.provider_id).cloned();
        let max_attempts = policy.as_ref().map_or(1, |p| u32::from(p.max_attempts));
        let timeout = self.timeouts.get(&candidate.provider_id).copied();

        let mut attempt = 1u32;
        loop {
            let started = Instant::now();
            let result = match timeout {
                Some(limit) => match tokio::time::timeout(limit, call()).await {
                    Ok(result) => result,
                    Err(_) => Err(ProviderError::Timeout(format!(
                        "provider '{}' exceeded timeout of {}ms",
                        candidate.provider_id,
                        limit.as_millis()
                    ))),
                },
                None => call().await,
            };
            let latency_ms = started.elapsed().as_millis() as u64;

            match result {
                Ok(value) => {
                    tracing::info!(
                        provider = %candidate.provider_id,
                        model = %model,
                        attempt,
                        latency_ms,
                        "{}",
                        success_message
                    );
                    return Ok(value);
                }
                Err(err) => {
                    if attempt < max_attempts && is_retryable(&err) {
                        let delay = policy
                            .as_ref()
                            .map_or(Duration::ZERO, |p| p.backoff_delay(attempt));
                        tracing::warn!(
                            provider = %candidate.provider_id,
                            model = %model,
                            attempt,
                            latency_ms,
                            backoff_ms = delay.as_millis() as u64,
                            error = %err,
                            "retryable provider error, retrying"
                        );
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    let reason = if is_retryable(&err) {
                        "retry exhausted, falling back"
                    } else {
                        "non-retryable, falling back"
                    };
                    tracing::warn!(
                        provider = %candidate.provider_id,
                        model = %model,
                        attempt,
                        latency_ms,
                        error = %err,
                        reason,
                        "provider failed"
                    );
                    return Err(err);
                }
            }
        }
    }

    /// Start a streaming chat completion, routing through aliases and fallbacks.
    pub async fn chat(
        &self,
        model_ref: &str,
        mut req: ChatRequest,
    ) -> Result<ChatStream, ProviderError> {
        let chain = self.resolve_chain(model_ref)?;
        if chain.is_empty() {
            return Err(ProviderError::AllProvidersFailed);
        }

        let mut last_error: Option<ProviderError> = None;
        for candidate in chain {
            let provider = self
                .providers
                .get(&candidate.provider_id)
                .cloned()
                .ok_or_else(|| ProviderError::ProviderNotFound(candidate.provider_id.clone()))?;

            req.model = candidate.model_name.clone();
            let model_key = format!("{}/{}", candidate.provider_id, candidate.model_name);
            let input_tokens = estimate_chat_tokens(&req);

            if let Err(err) = self
                .rate_limiter
                .acquire(&candidate.provider_id, input_tokens)
                .await
            {
                tracing::warn!(
                    provider = %candidate.provider_id,
                    model = %req.model,
                    error = %err,
                    "rate limited, trying next fallback"
                );
                last_error = Some(err);
                continue;
            }

            match self
                .call_with_retry(&candidate, &req.model, "provider chat started", || {
                    provider.chat(req.clone())
                })
                .await
            {
                Ok(stream) => {
                    // The per-request timeout in `call_with_retry` only covers
                    // stream establishment, so apply the same budget as a
                    // per-chunk idle timeout to bound mid-stream stalls.
                    let timeout = self.timeouts.get(&candidate.provider_id).copied();
                    let stream = http::with_idle_timeout(stream, timeout, &candidate.provider_id);
                    return Ok(track_chat_cost(
                        stream,
                        self.cost.clone(),
                        model_key,
                        input_tokens,
                    ));
                }
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.unwrap_or(ProviderError::AllProvidersFailed))
    }

    /// Generate embeddings through the routed provider.
    pub async fn embed(
        &self,
        model_ref: &str,
        mut req: EmbedRequest,
    ) -> Result<Vec<Embedding>, ProviderError> {
        let chain = self.resolve_chain(model_ref)?;
        if chain.is_empty() {
            return Err(ProviderError::AllProvidersFailed);
        }

        let mut last_error: Option<ProviderError> = None;
        for candidate in chain {
            let provider = self
                .providers
                .get(&candidate.provider_id)
                .cloned()
                .ok_or_else(|| ProviderError::ProviderNotFound(candidate.provider_id.clone()))?;

            req.model = candidate.model_name.clone();
            let model_key = format!("{}/{}", candidate.provider_id, candidate.model_name);
            let input_tokens: u64 = req.input.iter().map(|s| estimate_tokens(s)).sum();

            if let Err(err) = self
                .rate_limiter
                .acquire(&candidate.provider_id, input_tokens)
                .await
            {
                tracing::warn!(
                    provider = %candidate.provider_id,
                    model = %req.model,
                    error = %err,
                    "rate limited, trying next fallback"
                );
                last_error = Some(err);
                continue;
            }

            match self
                .call_with_retry(&candidate, &req.model, "provider embed completed", || {
                    provider.embed(req.clone())
                })
                .await
            {
                Ok(embeddings) => {
                    self.cost.record(&model_key, input_tokens, 0, true);
                    return Ok(embeddings);
                }
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.unwrap_or(ProviderError::AllProvidersFailed))
    }

    /// Plain fallback loop for one-shot, non-streaming provider calls
    /// (image, speech, video): unlike `chat`/`embed` there is no retry,
    /// rate-limit, or cost accounting — the first success wins, otherwise
    /// the last provider's error surfaces.
    async fn one_shot<Req, Resp, F, Fut>(
        &self,
        model_ref: &str,
        mut req: Req,
        op: &'static str,
        describe: fn(&Resp) -> String,
        mut call: F,
    ) -> Result<Resp, ProviderError>
    where
        Req: OneShotRequest,
        F: FnMut(Arc<dyn Provider>, Req) -> Fut,
        Fut: std::future::Future<Output = Result<Resp, ProviderError>>,
    {
        let chain = self.resolve_chain(model_ref)?;
        if chain.is_empty() {
            return Err(ProviderError::AllProvidersFailed);
        }

        let mut last_error: Option<ProviderError> = None;
        for candidate in chain {
            let provider = self
                .providers
                .get(&candidate.provider_id)
                .cloned()
                .ok_or_else(|| ProviderError::ProviderNotFound(candidate.provider_id.clone()))?;

            req.set_model(candidate.model_name.clone());
            match call(provider, req.clone()).await {
                Ok(response) => {
                    tracing::info!(
                        provider = %candidate.provider_id,
                        model = %req.model(),
                        detail = %describe(&response),
                        "{}",
                        format!("provider {op} completed")
                    );
                    return Ok(response);
                }
                Err(err) => {
                    tracing::warn!(
                        provider = %candidate.provider_id,
                        model = %req.model(),
                        error = %err,
                        "{}",
                        format!("{op} failed, trying next fallback")
                    );
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(ProviderError::AllProvidersFailed))
    }

    /// Generate images through the routed provider.
    pub async fn generate_image(
        &self,
        model_ref: &str,
        req: ImageRequest,
    ) -> Result<ImageResponse, ProviderError> {
        self.one_shot(
            model_ref,
            req,
            "image generation",
            |r: &ImageResponse| format!("images={}", r.images.len()),
            |provider, req| async move { provider.generate_image(req).await },
        )
        .await
    }

    /// Synthesize speech through the routed provider.
    pub async fn synthesize_speech(
        &self,
        model_ref: &str,
        req: SpeechRequest,
    ) -> Result<SpeechResponse, ProviderError> {
        self.one_shot(
            model_ref,
            req,
            "speech synthesis",
            |r: &SpeechResponse| format!("audio_bytes={}", r.audio.len()),
            |provider, req| async move { provider.synthesize_speech(req).await },
        )
        .await
    }

    /// Generate videos through the routed provider.
    pub async fn generate_video(
        &self,
        model_ref: &str,
        req: VideoRequest,
    ) -> Result<VideoResponse, ProviderError> {
        self.one_shot(
            model_ref,
            req,
            "video generation",
            |r: &VideoResponse| format!("videos={}", r.videos.len()),
            |provider, req| async move { provider.generate_video(req).await },
        )
        .await
    }

    /// Resolve a model reference into an ordered list of provider/model candidates.
    ///
    /// The first entry is the primary resolution; subsequent entries come from
    /// the configured fallback chain.
    fn resolve_chain(&self, model_ref: &str) -> Result<Vec<ResolvedModelRef>, ProviderError> {
        let primary = resolve_model_ref(model_ref, &self.aliases)?;
        let mut chain = vec![primary];

        for fallback in &self.fallbacks {
            let resolved = parse_model_ref(fallback)?;
            if !chain.contains(&resolved) {
                chain.push(resolved);
            }
        }

        Ok(chain)
    }

    /// Validate a model reference without starting a chat: it must resolve
    /// through aliases into a `provider/model` form whose provider is
    /// registered. Used to fail fast before launching work that would error
    /// on its first chat call anyway (e.g. sub-agent model overrides).
    pub fn validate_model_ref(&self, model_ref: &str) -> Result<(), ProviderError> {
        let primary = resolve_model_ref(model_ref, &self.aliases)?;
        if self.providers.contains_key(&primary.provider_id) {
            Ok(())
        } else {
            Err(ProviderError::ProviderNotFound(primary.provider_id))
        }
    }
}

/// Request kinds routed through [`ProviderRouter::one_shot`].
trait OneShotRequest: Clone {
    fn set_model(&mut self, model: String);
    fn model(&self) -> &str;
}

impl OneShotRequest for ImageRequest {
    fn set_model(&mut self, model: String) {
        self.model = model;
    }
    fn model(&self) -> &str {
        &self.model
    }
}

impl OneShotRequest for SpeechRequest {
    fn set_model(&mut self, model: String) {
        self.model = model;
    }
    fn model(&self) -> &str {
        &self.model
    }
}

impl OneShotRequest for VideoRequest {
    fn set_model(&mut self, model: String) {
        self.model = model;
    }
    fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatChunk, ChatMessage, FinishReason, GeneratedImage, ModelInfo};
    use async_trait::async_trait;
    use futures::{StreamExt, stream};
    use legion_core::config::{BackoffConfig, RateLimitConfig, RetryConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for FailingProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            Err(ProviderError::ProviderNotFound(self.id.clone()))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Err(ProviderError::EmbeddingNotSupported(self.id.clone()))
        }
    }

    struct OkProvider {
        id: String,
        model: String,
    }

    #[async_trait]
    impl Provider for OkProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo::new(&self.model, &self.id)]
        }

        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            assert_eq!(req.model, self.model);
            let chunk = ChatChunk {
                index: 0,
                delta: format!("from-{}", self.id),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(vec![Embedding {
                index: 0,
                embedding: vec![1.0],
            }])
        }
    }

    /// Fails the first `failures` chat/embed calls with either a retryable
    /// (`ProviderError::Timeout`) or a non-retryable error, then succeeds.
    struct FlakyProvider {
        id: String,
        failures_remaining: AtomicUsize,
        attempts: AtomicUsize,
        retryable: bool,
    }

    impl FlakyProvider {
        fn next_error(&self) -> ProviderError {
            if self.retryable {
                ProviderError::Timeout("boom".to_string())
            } else {
                ProviderError::ProviderNotFound(self.id.clone())
            }
        }

        fn should_fail(&self) -> bool {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let remaining = self.failures_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                self.failures_remaining
                    .store(remaining - 1, Ordering::SeqCst);
                return true;
            }
            false
        }
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            if self.should_fail() {
                return Err(self.next_error());
            }
            let chunk = ChatChunk {
                index: 0,
                delta: format!("from-{}", self.id),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            if self.should_fail() {
                return Err(self.next_error());
            }
            Ok(vec![Embedding {
                index: 0,
                embedding: vec![1.0],
            }])
        }
    }

    /// Sleeps before responding, used to exercise `timeout_seconds`.
    struct SlowProvider {
        id: String,
        delay: Duration,
        attempts: AtomicUsize,
    }

    #[async_trait]
    impl Provider for SlowProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            let chunk = ChatChunk {
                index: 0,
                delta: format!("from-{}", self.id),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Err(ProviderError::EmbeddingNotSupported(self.id.clone()))
        }
    }

    /// Yields one chunk and then fails mid-flight, used to verify that an
    /// errored stream never records cost.
    struct MidFlightErrorProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for MidFlightErrorProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let chunk = ChatChunk {
                index: 0,
                delta: "partial".to_string(),
                finish_reason: None,
                tool_calls: None,
            };
            Ok(Box::pin(stream::iter(vec![
                Ok(chunk),
                Err(ProviderError::StreamAborted(
                    "mid-flight failure".to_string(),
                )),
            ])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Err(ProviderError::EmbeddingNotSupported(self.id.clone()))
        }
    }

    /// Succeeds at image generation; chat/embed report unsupported.
    struct ImageOkProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for ImageOkProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            Err(ProviderError::ProviderNotFound(self.id.clone()))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Err(ProviderError::EmbeddingNotSupported(self.id.clone()))
        }

        async fn generate_image(&self, _req: ImageRequest) -> Result<ImageResponse, ProviderError> {
            Ok(ImageResponse {
                images: vec![GeneratedImage {
                    url: Some("https://example.com/img.png".to_string()),
                    b64_json: None,
                }],
            })
        }
    }

    fn fixed_retry(id: &str, max_attempts: u8) -> (String, RetryPolicy) {
        (
            id.to_string(),
            RetryPolicy::from_config(&RetryConfig {
                max_attempts,
                backoff: BackoffConfig::Fixed { ms: 0 },
            }),
        )
    }

    async fn first_chunk(stream: &mut ChatStream) -> ChatChunk {
        stream.next().await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn routes_to_primary_provider() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(OkProvider {
            id: "openai".to_string(),
            model: "gpt-4".to_string(),
        }));

        let req = ChatRequest::new("openai/gpt-4", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("openai/gpt-4", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-openai");
    }

    #[tokio::test]
    async fn resolves_alias_before_routing() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(OkProvider {
            id: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        }));
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        );
        router.set_aliases(aliases);

        let req = ChatRequest::new("claude", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("claude", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-anthropic");
    }

    #[test]
    fn validate_model_ref_checks_alias_and_provider() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(OkProvider {
            id: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        }));
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        );
        router.set_aliases(aliases);

        // Explicit provider/model refs and aliases both validate.
        assert!(
            router
                .validate_model_ref("anthropic/claude-sonnet-4-6")
                .is_ok()
        );
        assert!(router.validate_model_ref("claude").is_ok());
        // Unknown aliases, unregistered providers, and empty refs are rejected.
        assert!(router.validate_model_ref("default").is_err());
        assert!(router.validate_model_ref("ghost/model").is_err());
        assert!(router.validate_model_ref("").is_err());
    }

    #[tokio::test]
    async fn falls_back_when_primary_fails() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(FailingProvider {
            id: "primary".to_string(),
        }));
        router.register_provider(Arc::new(OkProvider {
            id: "fallback".to_string(),
            model: "fb-model".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/fb-model".to_string()]);

        let req = ChatRequest::new("primary/any", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("primary/any", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-fallback");
    }

    #[tokio::test]
    async fn returns_error_when_all_fail() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(FailingProvider {
            id: "primary".to_string(),
        }));
        router.register_provider(Arc::new(FailingProvider {
            id: "fallback".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/any".to_string()]);

        let req = ChatRequest::new("primary/any", vec![ChatMessage::user("hi")]);
        assert!(router.chat("primary/any", req).await.is_err());
    }

    #[tokio::test]
    async fn retries_retryable_error_within_provider() {
        let flaky = Arc::new(FlakyProvider {
            id: "primary".to_string(),
            failures_remaining: AtomicUsize::new(1),
            attempts: AtomicUsize::new(0),
            retryable: true,
        });
        let mut router = ProviderRouter::new();
        router.register_provider(flaky.clone());
        let (k, v) = fixed_retry("primary", 3);
        router.retry.insert(k, v);

        let req = ChatRequest::new("primary/any", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("primary/any", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-primary");
        assert_eq!(flaky.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn non_retryable_error_falls_back_without_retry() {
        let flaky = Arc::new(FlakyProvider {
            id: "primary".to_string(),
            failures_remaining: AtomicUsize::new(100),
            attempts: AtomicUsize::new(0),
            retryable: false,
        });
        let mut router = ProviderRouter::new();
        router.register_provider(flaky.clone());
        router.register_provider(Arc::new(OkProvider {
            id: "fallback".to_string(),
            model: "fb-model".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/fb-model".to_string()]);
        let (k, v) = fixed_retry("primary", 3);
        router.retry.insert(k, v);

        let req = ChatRequest::new("primary/any", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("primary/any", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-fallback");
        assert_eq!(flaky.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn falls_back_only_after_retry_exhausted() {
        let flaky = Arc::new(FlakyProvider {
            id: "primary".to_string(),
            failures_remaining: AtomicUsize::new(100),
            attempts: AtomicUsize::new(0),
            retryable: true,
        });
        let mut router = ProviderRouter::new();
        router.register_provider(flaky.clone());
        router.register_provider(Arc::new(OkProvider {
            id: "fallback".to_string(),
            model: "fb-model".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/fb-model".to_string()]);
        let (k, v) = fixed_retry("primary", 2);
        router.retry.insert(k, v);

        let req = ChatRequest::new("primary/any", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("primary/any", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-fallback");
        assert_eq!(flaky.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_seconds_applies_and_retries_then_falls_back() {
        let slow = Arc::new(SlowProvider {
            id: "slow".to_string(),
            delay: Duration::from_secs(10),
            attempts: AtomicUsize::new(0),
        });
        let mut router = ProviderRouter::new();
        router.register_provider(slow.clone());
        router.register_provider(Arc::new(OkProvider {
            id: "fallback".to_string(),
            model: "fb-model".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/fb-model".to_string()]);
        router
            .timeouts
            .insert("slow".to_string(), Duration::from_millis(50));
        let (k, v) = fixed_retry("slow", 2);
        router.retry.insert(k, v);

        let req = ChatRequest::new("slow/any", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("slow/any", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-fallback");
        // Timed out once, retried once (Timeout is retryable), then fell back.
        assert_eq!(slow.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_records_cost_after_stream_completes() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(OkProvider {
            id: "openai".to_string(),
            model: "gpt-4".to_string(),
        }));
        let mut rates = HashMap::new();
        rates.insert(
            "gpt-4".to_string(),
            ModelCost {
                input_per_1k: 0.01,
                output_per_1k: 0.03,
            },
        );
        router.cost = Arc::new(CostTracker::new(rates, None));

        let req = ChatRequest::new("openai/gpt-4", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("openai/gpt-4", req).await.unwrap();
        while stream.next().await.is_some() {}

        let snapshot = router.cost_tracker().snapshot();
        let stats = snapshot.models.get("openai/gpt-4").unwrap();
        assert_eq!(stats.calls, 1);
        assert_eq!(stats.estimated_calls, 1);
        assert!(stats.input_tokens > 0);
        assert!(stats.output_tokens > 0);
        assert!(stats.cost_usd > 0.0);
    }

    #[tokio::test]
    async fn embed_retries_and_records_cost() {
        let flaky = Arc::new(FlakyProvider {
            id: "primary".to_string(),
            failures_remaining: AtomicUsize::new(1),
            attempts: AtomicUsize::new(0),
            retryable: true,
        });
        let mut router = ProviderRouter::new();
        router.register_provider(flaky.clone());
        let (k, v) = fixed_retry("primary", 3);
        router.retry.insert(k, v);

        let req = EmbedRequest {
            model: "ignored".to_string(),
            input: vec!["hello world".to_string()],
            extra: HashMap::new(),
        };
        let embeddings = router.embed("primary/emb", req).await.unwrap();
        assert_eq!(embeddings.len(), 1);
        assert_eq!(flaky.attempts.load(Ordering::SeqCst), 2);

        let snapshot = router.cost_tracker().snapshot();
        let stats = snapshot.models.get("primary/emb").unwrap();
        assert_eq!(stats.calls, 1);
        assert!(stats.input_tokens > 0);
    }

    #[test]
    fn from_configs_reads_default_model_and_ops_config() {
        use legion_core::config::ProviderConfig;

        let config: ProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "minimax-openai",
            "kind": "openai",
            "baseUrl": "https://api.minimaxi.com/v1",
            "authProfile": "minimax-default",
            "timeoutSeconds": 120,
            "defaultModel": "MiniMax-M3",
            "retry": {
                "maxAttempts": 5,
                "backoff": { "type": "fixed", "ms": 250 }
            },
            "rateLimit": { "rpm": 60 }
        }))
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert("minimax-openai".to_string(), config);

        let mut auth = HashMap::new();
        auth.insert("minimax-default".to_string(), AuthProfile::api_key("test"));

        let router = ProviderRouter::from_configs(&configs, &auth, &HashMap::new(), None).unwrap();
        let models = router
            .providers
            .get("minimax-openai")
            .unwrap()
            .supported_models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "MiniMax-M3");

        let retry = router.retry.get("minimax-openai").unwrap();
        assert_eq!(retry.max_attempts, 5);
        assert_eq!(retry.backoff_delay(1), Duration::from_millis(250));
        assert!(router.rate_limiter.is_configured("minimax-openai"));
        assert_eq!(
            router.timeouts.get("minimax-openai"),
            Some(&Duration::from_secs(120))
        );
    }

    #[test]
    fn from_configs_builds_gemini_provider() {
        use legion_core::config::ProviderConfig;

        let config: ProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "gemini",
            "kind": "gemini",
            "authProfile": "gemini-default",
            "defaultModel": "gemini-2.5-flash"
        }))
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert("gemini".to_string(), config);

        let mut auth = HashMap::new();
        auth.insert("gemini-default".to_string(), AuthProfile::api_key("test"));

        let router = ProviderRouter::from_configs(&configs, &auth, &HashMap::new(), None).unwrap();
        let models = router.providers.get("gemini").unwrap().supported_models();
        // Static catalog (3) plus the default model is already in the catalog.
        assert_eq!(models.len(), 3);
        assert!(models.iter().any(|m| m.id == "gemini-2.5-flash"));
    }

    #[test]
    fn from_configs_builds_ollama_provider_without_auth_profile() {
        use legion_core::config::ProviderConfig;

        let config: ProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "ollama",
            "kind": "ollama",
            "baseUrl": "http://localhost:11434",
            "authProfile": "ollama-default",
            "defaultModel": "llama3"
        }))
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert("ollama".to_string(), config);

        // The referenced profile is absent from the map, so it resolves to an
        // empty key — fine for the local, unauthenticated Ollama provider.
        let router =
            ProviderRouter::from_configs(&configs, &HashMap::new(), &HashMap::new(), None).unwrap();
        let models = router.providers.get("ollama").unwrap().supported_models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama3");
    }

    #[test]
    fn from_configs_builds_bedrock_provider() {
        use legion_core::config::ProviderConfig;

        let config: ProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "bedrock",
            "kind": "bedrock",
            "authProfile": "aws-default",
            "defaultModel": "anthropic.claude-sonnet-4-5"
        }))
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert("bedrock".to_string(), config);

        let mut auth = HashMap::new();
        auth.insert(
            "aws-default".to_string(),
            AuthProfile::aws_sigv4("AKIDEXAMPLE", "secret", None, "us-east-1"),
        );

        let router = ProviderRouter::from_configs(&configs, &auth, &HashMap::new(), None).unwrap();
        let models = router.providers.get("bedrock").unwrap().supported_models();
        // Static catalog (2 chat + 1 embed); the default is already cataloged.
        assert_eq!(models.len(), 3);
        assert!(models.iter().any(|m| m.id == "anthropic.claude-sonnet-4-5"));
    }

    #[test]
    fn from_configs_rejects_bedrock_without_sigv4_profile() {
        use legion_core::config::ProviderConfig;

        let config: ProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "bedrock",
            "kind": "bedrock",
            "authProfile": "missing-profile"
        }))
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert("bedrock".to_string(), config);

        // The missing profile resolves to an api_key with an empty key, which
        // Bedrock rejects because it needs AWS SigV4 credentials.
        let result = ProviderRouter::from_configs(&configs, &HashMap::new(), &HashMap::new(), None);
        match result {
            Err(ProviderError::InvalidAuth(_)) => {}
            Err(err) => panic!("expected InvalidAuth, got {err}"),
            Ok(_) => panic!("bedrock without aws_sigv4 credentials must fail"),
        }
    }

    #[test]
    fn from_configs_rejects_gemini_without_api_key() {
        use legion_core::config::ProviderConfig;

        let config: ProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "gemini",
            "kind": "gemini",
            "authProfile": "missing-profile"
        }))
        .unwrap();

        let mut configs = HashMap::new();
        configs.insert("gemini".to_string(), config);

        // The missing profile resolves to an empty key, which Gemini rejects.
        let result = ProviderRouter::from_configs(&configs, &HashMap::new(), &HashMap::new(), None);
        match result {
            Err(ProviderError::InvalidAuth(_)) => {}
            Err(err) => panic!("expected InvalidAuth, got {err}"),
            Ok(_) => panic!("gemini without a key must fail"),
        }
    }

    #[tokio::test]
    async fn dropped_stream_records_no_cost() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(OkProvider {
            id: "openai".to_string(),
            model: "gpt-4".to_string(),
        }));

        let req = ChatRequest::new("openai/gpt-4", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("openai/gpt-4", req).await.unwrap();
        // Consume one chunk, then drop the stream before it finishes.
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-openai");
        drop(stream);

        let snapshot = router.cost_tracker().snapshot();
        assert!(
            snapshot.models.is_empty(),
            "a dropped stream must not record cost: {snapshot:?}"
        );
    }

    #[tokio::test]
    async fn errored_stream_records_no_cost() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(MidFlightErrorProvider {
            id: "flaky-stream".to_string(),
        }));

        let req = ChatRequest::new("flaky-stream/any", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("flaky-stream/any", req).await.unwrap();
        // Consume until the mid-flight error, then stop — as the agent loop
        // does when a stream fails.
        let mut saw_error = false;
        while let Some(item) = stream.next().await {
            if item.is_err() {
                saw_error = true;
                break;
            }
        }
        assert!(saw_error, "the stream must fail mid-flight");
        drop(stream);

        let snapshot = router.cost_tracker().snapshot();
        assert!(
            snapshot.models.is_empty(),
            "an errored stream must not record cost: {snapshot:?}"
        );
    }

    #[tokio::test]
    async fn generate_image_falls_back_to_second_provider() {
        let mut router = ProviderRouter::new();
        // The primary uses the default `generate_image` (ImageNotSupported).
        router.register_provider(Arc::new(FailingProvider {
            id: "primary".to_string(),
        }));
        router.register_provider(Arc::new(ImageOkProvider {
            id: "fallback".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/img-model".to_string()]);

        let req = ImageRequest {
            model: "ignored".to_string(),
            prompt: "a cat".to_string(),
            size: None,
            n: None,
        };
        let resp = router
            .generate_image("primary/img-model", req)
            .await
            .unwrap();
        assert_eq!(resp.images.len(), 1);
        assert_eq!(
            resp.images[0].url.as_deref(),
            Some("https://example.com/img.png")
        );
    }

    #[tokio::test]
    async fn synthesize_speech_returns_last_error_when_all_fail() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(FailingProvider {
            id: "primary".to_string(),
        }));
        router.register_provider(Arc::new(FailingProvider {
            id: "fallback".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/tts-model".to_string()]);

        let req = SpeechRequest {
            model: "ignored".to_string(),
            input: "hello".to_string(),
            voice: None,
            format: None,
        };
        let err = router
            .synthesize_speech("primary/tts-model", req)
            .await
            .unwrap_err();
        // The chain exhausts and surfaces the LAST provider's error, not
        // AllProvidersFailed.
        assert!(matches!(
            err,
            ProviderError::SpeechNotSupported(id) if id == "fallback"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_primary_falls_back() {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(OkProvider {
            id: "primary".to_string(),
            model: "p-model".to_string(),
        }));
        router.rate_limiter.configure(
            "primary",
            &RateLimitConfig {
                rpm: Some(1),
                tpm: None,
            },
        );
        // Drain the primary's single-request burst; time is paused, so the
        // bucket never refills.
        router.rate_limiter.acquire("primary", 0).await.unwrap();

        // With no fallback configured the rate-limit error surfaces directly.
        let req = ChatRequest::new("primary/p-model", vec![ChatMessage::user("hi")]);
        match router.chat("primary/p-model", req).await {
            Err(err) => assert!(matches!(
                err,
                ProviderError::RateLimited(id) if id == "primary"
            )),
            Ok(_) => panic!("rate-limited primary without fallback must fail"),
        }

        // With a fallback configured the rate-limited primary is skipped.
        router.register_provider(Arc::new(OkProvider {
            id: "fallback".to_string(),
            model: "fb-model".to_string(),
        }));
        router.set_fallbacks(vec!["fallback/fb-model".to_string()]);

        let req = ChatRequest::new("primary/p-model", vec![ChatMessage::user("hi")]);
        let mut stream = router.chat("primary/p-model", req).await.unwrap();
        let chunk = first_chunk(&mut stream).await;
        assert_eq!(chunk.delta, "from-fallback");
    }
}
