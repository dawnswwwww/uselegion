//! Operational concerns for provider calls: retry classification and backoff,
//! per-provider rate limiting, and per-model cost accounting.

use crate::types::{ChatRequest, ChatStream, ProviderError};
use futures::{StreamExt, stream};
use legion_core::config::{BackoffConfig, ModelCost, RateLimitConfig, RetryConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

/// Maximum total time a single `acquire` may spend waiting for rate-limit
/// tokens before giving up with [`ProviderError::RateLimited`].
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);

/// Shared cl100k_base encoder. Building the BPE (≈50k merge rules, sorted on
/// construction) costs hundreds of milliseconds, so it must happen once per
/// process — not per call, as token estimation sits on the hot path of every
/// provider request.
pub fn cl100k_bpe() -> Option<&'static tiktoken_rs::CoreBPE> {
    static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

/// Estimate tokens in `text` with the cl100k_base tokenizer (GPT-4 family);
/// falls back to a character heuristic if the BPE data fails to load.
pub fn estimate_tokens(text: &str) -> u64 {
    match cl100k_bpe() {
        Some(bpe) => bpe.encode_with_special_tokens(text).len() as u64,
        None => (text.chars().count() / 4 + 1) as u64,
    }
}

/// Rough input-token estimate for a chat request (message contents + tool docs).
pub fn estimate_chat_tokens(req: &ChatRequest) -> u64 {
    let messages: u64 = req
        .messages
        .iter()
        .map(|m| estimate_tokens(&m.content))
        .sum();
    let tools: u64 = req
        .tools
        .as_ref()
        .map(|defs| {
            defs.iter()
                .map(|d| estimate_tokens(&d.name) + estimate_tokens(&d.description))
                .sum()
        })
        .unwrap_or(0);
    messages + tools
}

/// Classify whether a provider error is transient and worth retrying inside
/// the same provider before falling back to the next candidate: HTTP 429/5xx,
/// connection/transport timeouts, and router-level call timeouts.
pub fn is_retryable(err: &ProviderError) -> bool {
    match err {
        ProviderError::Http(http) => {
            http.is_timeout()
                || http.is_connect()
                || http
                    .status()
                    .is_some_and(|s| s.as_u16() == 429 || s.is_server_error())
        }
        ProviderError::Timeout(_) => true,
        _ => false,
    }
}

/// Runtime retry policy derived from [`RetryConfig`].
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub backoff: BackoffConfig,
}

impl RetryPolicy {
    pub fn from_config(config: &RetryConfig) -> Self {
        Self {
            max_attempts: config.max_attempts.max(1),
            backoff: config.backoff.clone(),
        }
    }

    /// Delay before attempt `attempt + 1` (attempts are 1-based: the delay
    /// returned for attempt 1 precedes the second call).
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        match &self.backoff {
            BackoffConfig::Exponential { base_ms, max_ms } => {
                let factor = 1u64
                    .checked_shl(attempt.saturating_sub(1))
                    .unwrap_or(u64::MAX);
                Duration::from_millis(base_ms.saturating_mul(factor).min(*max_ms))
            }
            BackoffConfig::Fixed { ms } => Duration::from_millis(*ms),
        }
    }
}

/// Token bucket with `capacity` tokens, refilling at `capacity` per minute.
/// Uses `tokio::time::Instant` so paused-clock tests advance refills.
#[derive(Debug)]
struct Bucket {
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl Bucket {
    fn new(capacity: f64) -> Self {
        Self {
            capacity,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    /// Seconds until `needed` tokens are available (refilling first).
    /// Returns `f64::MAX` for a zero-capacity bucket (can never satisfy).
    fn seconds_until(&mut self, needed: f64) -> f64 {
        if self.capacity <= 0.0 {
            return f64::MAX;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.capacity / 60.0).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= needed {
            0.0
        } else {
            (needed - self.tokens) * 60.0 / self.capacity
        }
    }

    fn consume(&mut self, amount: f64) {
        self.tokens -= amount;
    }
}

#[derive(Debug, Default)]
struct ProviderBuckets {
    rpm: Option<Bucket>,
    tpm: Option<Bucket>,
}

/// Per-provider token-bucket rate limiter (requests/minute and tokens/minute).
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, ProviderBuckets>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure (or replace) the limits for a provider.
    pub fn configure(&self, provider_id: &str, config: &RateLimitConfig) {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        buckets.insert(
            provider_id.to_string(),
            ProviderBuckets {
                rpm: config.rpm.map(|rpm| Bucket::new(f64::from(rpm))),
                tpm: config.tpm.map(|tpm| Bucket::new(f64::from(tpm))),
            },
        );
    }

    /// Whether any limits are configured for this provider.
    pub fn is_configured(&self, provider_id: &str) -> bool {
        self.buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(provider_id)
    }

    /// Wait until one request plus `est_tokens` tokens fit the configured
    /// buckets, then consume them. Fails with [`ProviderError::RateLimited`]
    /// when the total wait would exceed 30 seconds.
    pub async fn acquire(&self, provider_id: &str, est_tokens: u64) -> Result<(), ProviderError> {
        let mut waited = Duration::ZERO;
        loop {
            let wait_secs = {
                let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
                match buckets.get_mut(provider_id) {
                    Some(provider) => {
                        let rpm_wait = provider
                            .rpm
                            .as_mut()
                            .map(|b| b.seconds_until(1.0))
                            .unwrap_or(0.0);
                        let tpm_wait = provider
                            .tpm
                            .as_mut()
                            .map(|b| b.seconds_until(est_tokens as f64))
                            .unwrap_or(0.0);
                        rpm_wait.max(tpm_wait)
                    }
                    None => return Ok(()),
                }
            };

            if wait_secs <= 0.0 {
                let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(provider) = buckets.get_mut(provider_id) {
                    if let Some(rpm) = provider.rpm.as_mut() {
                        rpm.consume(1.0);
                    }
                    if let Some(tpm) = provider.tpm.as_mut() {
                        tpm.consume(est_tokens as f64);
                    }
                }
                return Ok(());
            }

            // Cap the conversion: any wait above the budget errors out below,
            // so the exact duration past 60s does not matter.
            let wait = Duration::from_secs_f64(wait_secs.min(60.0));
            if waited + wait > MAX_RATE_LIMIT_WAIT {
                return Err(ProviderError::RateLimited(provider_id.to_string()));
            }
            tokio::time::sleep(wait).await;
            waited += wait;
        }
    }
}

/// Accumulated cost stats for one model key.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostStats {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// How many calls were token-estimated (no provider usage data).
    pub estimated_calls: u64,
}

/// Serializable snapshot of all accumulated cost stats.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CostSnapshot {
    pub models: HashMap<String, ModelCostStats>,
    pub total_cost_usd: f64,
}

/// Tracks per-model token usage and dollar cost, optionally write-through
/// persisted to a JSON file after every record.
#[derive(Debug)]
pub struct CostTracker {
    rates: HashMap<String, ModelCost>,
    inner: Mutex<HashMap<String, ModelCostStats>>,
    persist_path: Option<PathBuf>,
}

impl CostTracker {
    /// Create a tracker, loading any previously persisted snapshot from
    /// `persist_path` (corrupt or unreadable files start fresh with a warning).
    pub fn new(rates: HashMap<String, ModelCost>, persist_path: Option<PathBuf>) -> Self {
        let mut initial = HashMap::new();
        if let Some(path) = &persist_path {
            if path.exists() {
                let loaded = std::fs::read_to_string(path)
                    .map_err(|e| e.to_string())
                    .and_then(|text| {
                        serde_json::from_str::<CostSnapshot>(&text).map_err(|e| e.to_string())
                    });
                match loaded {
                    Ok(snapshot) => initial = snapshot.models,
                    Err(err) => tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "failed to load persisted cost snapshot, starting fresh"
                    ),
                }
            }
        }
        Self {
            rates,
            inner: Mutex::new(initial),
            persist_path,
        }
    }

    /// Fully-qualified `"<provider>/<model>"` keys win over bare `"<model>"`.
    fn rate_for(&self, model_key: &str) -> Option<ModelCost> {
        self.rates.get(model_key).copied().or_else(|| {
            model_key
                .rsplit('/')
                .next()
                .and_then(|bare| self.rates.get(bare).copied())
        })
    }

    /// Record one call. Models without a configured rate are still counted
    /// with a zero dollar cost.
    pub fn record(&self, model_key: &str, input_tokens: u64, output_tokens: u64, estimated: bool) {
        let cost = self.rate_for(model_key).map_or(0.0, |rate| {
            (input_tokens as f64 / 1000.0) * rate.input_per_1k
                + (output_tokens as f64 / 1000.0) * rate.output_per_1k
        });

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = inner.entry(model_key.to_string()).or_default();
        entry.calls += 1;
        entry.input_tokens += input_tokens;
        entry.output_tokens += output_tokens;
        entry.cost_usd += cost;
        if estimated {
            entry.estimated_calls += 1;
        }

        if let Some(path) = &self.persist_path {
            let snapshot = CostSnapshot {
                total_cost_usd: inner.values().map(|s| s.cost_usd).sum(),
                models: inner.clone(),
            };
            match serde_json::to_string_pretty(&snapshot) {
                Ok(text) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(err) = legion_core::fs::atomic_write(path, text.as_bytes()) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "failed to persist cost snapshot"
                        );
                    }
                }
                Err(err) => tracing::warn!(error = %err, "failed to serialize cost snapshot"),
            }
        }
    }

    /// Current accumulated stats.
    pub fn snapshot(&self) -> CostSnapshot {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        CostSnapshot {
            total_cost_usd: inner.values().map(|s| s.cost_usd).sum(),
            models: inner.clone(),
        }
    }
}

/// Wrap a chat stream so its accumulated output text is cost-tracked when the
/// stream finishes normally (yields `None`). Output tokens are tiktoken
/// estimates, so the record is flagged `estimated`. If the stream is dropped
/// early or errors mid-flight, no cost record is written for it.
pub fn track_chat_cost(
    stream: ChatStream,
    tracker: Arc<CostTracker>,
    model_key: String,
    input_tokens: u64,
) -> ChatStream {
    struct State {
        stream: ChatStream,
        tracker: Arc<CostTracker>,
        model_key: String,
        input_tokens: u64,
        delta: String,
        recorded: bool,
    }

    Box::pin(stream::unfold(
        State {
            stream,
            tracker,
            model_key,
            input_tokens,
            delta: String::new(),
            recorded: false,
        },
        |mut state| async move {
            match state.stream.next().await {
                Some(Ok(chunk)) => {
                    state.delta.push_str(&chunk.delta);
                    Some((Ok(chunk), state))
                }
                Some(Err(err)) => Some((Err(err), state)),
                None => {
                    if !state.recorded {
                        state.recorded = true;
                        let output_tokens = estimate_tokens(&state.delta);
                        state.tracker.record(
                            &state.model_key,
                            state.input_tokens,
                            output_tokens,
                            true,
                        );
                    }
                    None
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn http_status_error(status: u16) -> ProviderError {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(any())
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let resp = reqwest::get(server.uri()).await.expect("mock response");
            ProviderError::Http(resp.error_for_status().expect_err("error status"))
        })
    }

    #[test]
    fn retryable_classification() {
        assert!(is_retryable(&ProviderError::Timeout("t".to_string())));
        assert!(!is_retryable(&ProviderError::ProviderNotFound(
            "p".to_string()
        )));
        assert!(!is_retryable(&ProviderError::PromptTooLong));
        assert!(!is_retryable(&ProviderError::AllProvidersFailed));
        assert!(!is_retryable(&ProviderError::InvalidAuth("a".to_string())));
        assert!(is_retryable(&http_status_error(429)));
        assert!(is_retryable(&http_status_error(500)));
        assert!(!is_retryable(&http_status_error(404)));
    }

    #[tokio::test]
    async fn connect_errors_are_retryable() {
        // Port 1 refuses connections deterministically, producing a
        // connect-class reqwest error without any real network dependency.
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("connection refused");
        assert!(is_retryable(&ProviderError::Http(err)));
    }

    #[test]
    fn exponential_backoff_doubles_and_caps() {
        let policy = RetryPolicy::from_config(&RetryConfig {
            max_attempts: 6,
            backoff: BackoffConfig::Exponential {
                base_ms: 500,
                max_ms: 8000,
            },
        });
        assert_eq!(policy.backoff_delay(1), Duration::from_millis(500));
        assert_eq!(policy.backoff_delay(2), Duration::from_millis(1000));
        assert_eq!(policy.backoff_delay(3), Duration::from_millis(2000));
        assert_eq!(policy.backoff_delay(4), Duration::from_millis(4000));
        assert_eq!(policy.backoff_delay(5), Duration::from_millis(8000));
        assert_eq!(policy.backoff_delay(6), Duration::from_millis(8000));
        assert_eq!(policy.backoff_delay(100), Duration::from_millis(8000));
    }

    #[test]
    fn fixed_backoff_is_constant() {
        let policy = RetryPolicy::from_config(&RetryConfig {
            max_attempts: 3,
            backoff: BackoffConfig::Fixed { ms: 250 },
        });
        assert_eq!(policy.backoff_delay(1), Duration::from_millis(250));
        assert_eq!(policy.backoff_delay(9), Duration::from_millis(250));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waits_for_rpm_refill() {
        let limiter = RateLimiter::new();
        limiter.configure(
            "p",
            &RateLimitConfig {
                rpm: Some(60),
                tpm: None,
            },
        );
        // Drain the 60-request burst capacity; every call is instant while
        // tokens remain.
        for _ in 0..60 {
            limiter.acquire("p", 0).await.expect("burst acquire");
        }
        // The next call must wait ~1s for the bucket to refill one token.
        let started = Instant::now();
        limiter.acquire("p", 0).await.expect("refilled acquire");
        assert_eq!(started.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_gives_up_after_wait_budget() {
        let limiter = RateLimiter::new();
        limiter.configure(
            "p",
            &RateLimitConfig {
                rpm: Some(1),
                tpm: None,
            },
        );
        limiter.acquire("p", 0).await.expect("first acquire");
        let err = limiter
            .acquire("p", 0)
            .await
            .expect_err("must be rate limited");
        assert!(matches!(err, ProviderError::RateLimited(id) if id == "p"));
    }

    #[tokio::test(start_paused = true)]
    async fn tpm_limits_token_throughput() {
        let limiter = RateLimiter::new();
        limiter.configure(
            "p",
            &RateLimitConfig {
                rpm: None,
                tpm: Some(60),
            },
        );
        limiter.acquire("p", 60).await.expect("first acquire");
        let err = limiter
            .acquire("p", 60)
            .await
            .expect_err("tpm refill of 60s exceeds budget");
        assert!(matches!(err, ProviderError::RateLimited(_)));
    }

    #[tokio::test]
    async fn unconfigured_provider_passes_immediately() {
        let limiter = RateLimiter::new();
        limiter.acquire("nope", 1_000_000).await.expect("no limits");
        assert!(!limiter.is_configured("nope"));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_capacity_bucket_always_rate_limited() {
        let limiter = RateLimiter::new();
        limiter.configure(
            "p",
            &RateLimitConfig {
                rpm: Some(0),
                tpm: None,
            },
        );
        let err = limiter
            .acquire("p", 0)
            .await
            .expect_err("a zero-rpm bucket can never satisfy a request");
        assert!(matches!(err, ProviderError::RateLimited(id) if id == "p"));
    }

    #[test]
    fn retry_policy_clamps_zero_max_attempts() {
        let policy = RetryPolicy::from_config(&RetryConfig {
            max_attempts: 0,
            backoff: BackoffConfig::Fixed { ms: 0 },
        });
        assert_eq!(policy.max_attempts, 1);
    }

    fn rates(pairs: &[(&str, f64, f64)]) -> HashMap<String, ModelCost> {
        pairs
            .iter()
            .map(|(k, i, o)| {
                (
                    k.to_string(),
                    ModelCost {
                        input_per_1k: *i,
                        output_per_1k: *o,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn cost_record_math_and_qualified_key_precedence() {
        let tracker = CostTracker::new(
            rates(&[
                ("openai/gpt-4o", 0.01, 0.03),
                ("gpt-4o", 1.0, 1.0),
                ("claude", 0.003, 0.015),
            ]),
            None,
        );
        // Fully-qualified key wins over the bare key.
        tracker.record("openai/gpt-4o", 1000, 1000, true);
        // Bare-key fallback for an unqualified match.
        tracker.record("anthropic/claude", 2000, 1000, false);
        // Unknown model: counted with zero cost.
        tracker.record("local/llama", 500, 500, true);

        let snapshot = tracker.snapshot();
        let openai = snapshot.models.get("openai/gpt-4o").unwrap();
        assert_eq!(openai.calls, 1);
        assert_eq!(openai.input_tokens, 1000);
        assert_eq!(openai.output_tokens, 1000);
        assert!((openai.cost_usd - 0.04).abs() < 1e-9);
        assert_eq!(openai.estimated_calls, 1);

        let claude = snapshot.models.get("anthropic/claude").unwrap();
        assert!((claude.cost_usd - (0.006 + 0.015)).abs() < 1e-9);
        assert_eq!(claude.estimated_calls, 0);

        let llama = snapshot.models.get("local/llama").unwrap();
        assert_eq!(llama.calls, 1);
        assert_eq!(llama.cost_usd, 0.0);

        assert!((snapshot.total_cost_usd - (0.04 + 0.021)).abs() < 1e-9);
    }

    #[test]
    fn cost_persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("costs.json");
        {
            let tracker = CostTracker::new(rates(&[("m", 0.001, 0.002)]), Some(path.clone()));
            tracker.record("m", 1000, 500, true);
            tracker.record("m", 2000, 1000, false);
        }
        let reloaded = CostTracker::new(rates(&[("m", 0.001, 0.002)]), Some(path));
        let snapshot = reloaded.snapshot();
        let stats = snapshot.models.get("m").unwrap();
        assert_eq!(stats.calls, 2);
        assert_eq!(stats.input_tokens, 3000);
        assert_eq!(stats.output_tokens, 1500);
        assert_eq!(stats.estimated_calls, 1);
        // 3k in * 0.001 + 1.5k out * 0.002 = 0.003 + 0.003
        assert!((stats.cost_usd - 0.006).abs() < 1e-9);
    }

    #[test]
    fn corrupt_persist_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("costs.json");
        std::fs::write(&path, "not json").unwrap();
        let tracker = CostTracker::new(HashMap::new(), Some(path));
        assert!(tracker.snapshot().models.is_empty());
    }

    #[test]
    fn cost_persistence_is_atomic_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("costs.json");
        let tracker = CostTracker::new(rates(&[("m", 0.001, 0.002)]), Some(path.clone()));
        tracker.record("m", 1000, 500, true);
        tracker.record("m", 2000, 1000, false);

        // The target file holds the full, valid snapshot.
        let text = std::fs::read_to_string(&path).unwrap();
        let persisted: serde_json::Value = serde_json::from_str(&text).unwrap();
        let stats = &persisted["models"]["m"];
        assert_eq!(stats["calls"], 2);
        assert_eq!(stats["inputTokens"], 3000);

        // No temp residue remains in the same directory.
        let residue: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(residue.is_empty(), "temp files left behind: {residue:?}");
    }

    #[test]
    fn estimate_tokens_counts_text() {
        assert_eq!(estimate_tokens(""), 0);
        assert!(estimate_tokens("hello world") > 0);
        let req = ChatRequest::new("m", vec![crate::types::ChatMessage::user("hi there")]);
        assert!(estimate_chat_tokens(&req) > 0);
    }
}
