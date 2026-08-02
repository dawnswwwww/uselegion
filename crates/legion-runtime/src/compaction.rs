use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};

use futures::StreamExt;
use legion_provider::model_ref::parse_context_window_suffix;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{ChatMessage, ChatRequest, ChatRole, FinishReason};
use tracing::{info, warn};

use crate::context::SessionContext;
use crate::token_counter::estimate_total_tokens;
use crate::types::{BoundaryMark, CompactionResult, RuntimeError};
use legion_core::config::CompactionConfig;
use legion_core::util::iso_now;

const SUMMARY_SYSTEM_PROMPT: &str = r#"You are summarizing a conversation between a user and an AI coding assistant.
Produce a concise but information-dense summary of the conversation fragment below.
Preserve: key facts the user stated, decisions made, code changes or file paths mentioned,
errors encountered, and any pending tasks or follow-ups.
Omit routine greetings and verbatim tool-call syntax unless the output was important."#;

/// Circuit breaker that disables auto-compaction after a configurable number of
/// consecutive failures.
#[derive(Debug)]
pub struct CircuitBreaker {
    max: u8,
    failures: AtomicU8,
}

impl CircuitBreaker {
    /// Create a breaker that opens after `max` consecutive failures.
    pub fn new(max: u8) -> Self {
        Self {
            max,
            failures: AtomicU8::new(0),
        }
    }

    /// Returns `true` while the breaker is closed (failures < max).
    pub fn allow(&self) -> bool {
        self.failures.load(Ordering::SeqCst) < self.max
    }

    /// Reset the failure counter to zero.
    pub fn record_success(&self) {
        self.failures.store(0, Ordering::SeqCst);
    }

    /// Increment the failure counter. Returns `true` if the breaker just opened.
    pub fn record_failure(&self) -> bool {
        let previous = self.failures.fetch_add(1, Ordering::SeqCst);
        previous + 1 >= self.max
    }

    /// Current number of recorded consecutive failures.
    pub fn consecutive_failures(&self) -> u8 {
        self.failures.load(Ordering::SeqCst)
    }
}

/// Stateful conversation compactor.
///
/// Holds the compaction configuration and a circuit breaker so that repeated
/// compaction failures do not spam the provider with doomed summary requests.
#[derive(Debug)]
pub struct Compactor {
    config: CompactionConfig,
    breaker: CircuitBreaker,
}

impl Compactor {
    /// Create a new compactor from the supplied configuration.
    pub fn new(config: CompactionConfig) -> Self {
        let breaker = CircuitBreaker::new(config.max_consecutive_failures);
        Self { config, breaker }
    }

    /// Check whether compaction is needed and, if so, replace the oldest eligible
    /// messages in `messages` with a summary.
    ///
    /// On success, returns `Some(summary)` when compaction occurred and `None`
    /// when the window is still below the threshold.
    pub async fn compact_if_needed(
        &self,
        messages: &mut Vec<ChatMessage>,
        system_prompt: &str,
        provider: &ProviderRouter,
        model_ref: &str,
        session_ctx: Option<&SessionContext>,
        query: &str,
    ) -> Result<Option<(String, Option<BoundaryMark>)>, RuntimeError> {
        if !self.should_compact(messages, system_prompt, model_ref) {
            return Ok(None);
        }

        if !self.breaker.allow() {
            warn!(
                consecutive_failures = self.breaker.consecutive_failures(),
                "compaction circuit breaker is open; skipping auto-compaction"
            );
            return Ok(None);
        }

        let result = self
            .compact_conversation(
                messages.clone(),
                system_prompt,
                provider,
                model_ref,
                session_ctx,
                query,
            )
            .await;

        match result {
            Ok(outcome) => {
                self.breaker.record_success();
                if outcome.compacted {
                    info!(
                        tokens_before = outcome.tokens_before,
                        tokens_after = outcome.tokens_after,
                        summary_len = outcome.summary.len(),
                        reattachment_count = outcome.reattachments.len(),
                        consecutive_failures = self.breaker.consecutive_failures(),
                        "compaction succeeded"
                    );
                    let summary = outcome.summary.clone();
                    let boundary = outcome.boundary.clone();
                    *messages = outcome.messages;
                    Ok(Some((summary, boundary)))
                } else {
                    Ok(None)
                }
            }
            Err(err) => {
                let just_opened = self.breaker.record_failure();
                if just_opened {
                    warn!(
                        error = %err,
                        max_consecutive_failures = self.config.max_consecutive_failures,
                        "compaction circuit breaker opened due to repeated failures"
                    );
                } else {
                    warn!(
                        error = %err,
                        consecutive_failures = self.breaker.consecutive_failures(),
                        "compaction failed"
                    );
                }
                Err(err)
            }
        }
    }

    /// Resolve the effective context window for `model_ref`, honoring overrides
    /// in priority order:
    ///
    /// 1. A trailing model-name suffix (`minimax/MiniMax-M3[1m]`) — most specific.
    /// 2. A `compaction.context_windows` table entry keyed by the suffix-stripped
    ///    `provider/model`.
    /// 3. The global `compaction.context_window` fallback.
    ///
    /// Only `context_window` is overridden; `buffer_tokens` and `threshold_ratio`
    /// are always taken from the global config.
    pub fn effective_context_window(&self, model_ref: &str) -> usize {
        if let Some(win) = parse_context_window_suffix(model_ref).1 {
            return win;
        }
        if let Some(win) = self.config.context_windows.get(model_ref).copied() {
            return win;
        }
        self.config.context_window
    }

    /// Decide whether the current message list has grown large enough to compact.
    ///
    /// When `buffer_tokens` is configured, compaction triggers once the estimated
    /// tokens reach `effective_window - buffer_tokens`, leaving headroom for the
    /// next model turn. When `buffer_tokens` is zero, the legacy ratio threshold
    /// is used. `model_ref` selects the per-model context window (suffix or
    /// `context_windows` table override; falls back to the global value).
    pub fn should_compact(
        &self,
        messages: &[ChatMessage],
        system_prompt: &str,
        model_ref: &str,
    ) -> bool {
        if messages.len() < self.config.min_messages_to_keep.saturating_add(2) {
            return false;
        }

        let effective_window = self.effective_context_window(model_ref);
        let threshold = if self.config.buffer_tokens > 0 {
            effective_window.saturating_sub(self.config.buffer_tokens)
        } else {
            (effective_window as f32 * self.config.threshold_ratio) as usize
        };
        estimate_total_tokens(messages, system_prompt) >= threshold
    }

    /// Compact a conversation by summarizing its oldest eligible messages.
    ///
    /// The first system message (if present) is always preserved. The most recent
    /// `min_messages_to_keep` messages are also preserved. Everything in between is
    /// sent to a summary model and replaced by a single summary message.
    ///
    /// Tool-use invariants are guarded: a `tool_result` is never preserved without
    /// its matching `tool_use` assistant message.
    async fn compact_conversation(
        &self,
        messages: Vec<ChatMessage>,
        system_prompt: &str,
        provider: &ProviderRouter,
        model_ref: &str,
        session_ctx: Option<&SessionContext>,
        query: &str,
    ) -> Result<CompactionResult, RuntimeError> {
        let tokens_before = estimate_total_tokens(&messages, system_prompt);

        let boundary = select_compaction_boundary(&messages, self.config.min_messages_to_keep);
        if boundary == 0 {
            // Not enough material to compact safely.
            return Ok(CompactionResult {
                summary: String::new(),
                messages,
                tokens_before,
                tokens_after: tokens_before,
                compacted: false,
                reattachments: Vec::new(),
                boundary: None,
            });
        }

        let mut summary_source: Vec<ChatMessage> = messages[..boundary].to_vec();
        strip_attachments(
            &mut summary_source,
            self.config.strip_images,
            self.config.strip_documents,
        );

        let summary_model_ref = self.config.summary_model.as_deref().unwrap_or(model_ref);
        let summary = generate_summary(
            provider,
            summary_model_ref,
            &summary_source,
            self.config.max_summary_tokens,
        )
        .await?;

        if summary.is_empty() {
            return Ok(CompactionResult {
                summary: String::new(),
                messages,
                tokens_before,
                tokens_after: tokens_before,
                compacted: false,
                reattachments: Vec::new(),
                boundary: None,
            });
        }

        let compacted = build_compacted_messages(
            &messages,
            boundary,
            &summary,
            self.config.use_prompt_cache,
            session_ctx,
            query,
        )
        .await;
        let tokens_after = estimate_total_tokens(&compacted, system_prompt);

        let boundary_mark = BoundaryMark {
            entry_index: 0,
            timestamp_iso: iso_now(),
            tokens_compacted: tokens_before.saturating_sub(tokens_after),
        };

        Ok(CompactionResult {
            summary,
            messages: compacted,
            tokens_before,
            tokens_after,
            compacted: true,
            reattachments: Vec::new(),
            boundary: Some(boundary_mark),
        })
    }
}

/// Two-stage conversation compactor.
///
/// When enabled, it first summarizes the prefix of the compaction window, then
/// rewrites that prefix summary together with the remaining tail into a denser
/// final summary. If any stage fails or produces an empty result, the
/// implementation falls back to the single-stage [`Compactor`].
#[derive(Debug)]
pub struct TwoPassCompactor {
    inner: Compactor,
}

impl TwoPassCompactor {
    /// Create a new two-pass compactor from the supplied configuration.
    pub fn new(config: CompactionConfig) -> Self {
        Self {
            inner: Compactor::new(config),
        }
    }

    /// Check whether compaction is needed and, if so, replace the oldest eligible
    /// messages in `messages` with a summary.
    ///
    /// On success, returns `Some(summary)` when compaction occurred and `None`
    /// when the window is still below the threshold.
    pub async fn compact_if_needed(
        &self,
        messages: &mut Vec<ChatMessage>,
        system_prompt: &str,
        provider: &ProviderRouter,
        model_ref: &str,
        session_ctx: Option<&SessionContext>,
        query: &str,
    ) -> Result<Option<(String, Option<BoundaryMark>)>, RuntimeError> {
        if !self
            .inner
            .should_compact(messages, system_prompt, model_ref)
        {
            return Ok(None);
        }

        if !self.inner.breaker.allow() {
            warn!(
                consecutive_failures = self.inner.breaker.consecutive_failures(),
                "compaction circuit breaker is open; skipping auto-compaction"
            );
            return Ok(None);
        }

        if !self.inner.config.two_pass_enabled {
            return self
                .inner
                .compact_if_needed(
                    messages,
                    system_prompt,
                    provider,
                    model_ref,
                    session_ctx,
                    query,
                )
                .await;
        }

        match self
            .compact_conversation_two_pass(
                messages.clone(),
                system_prompt,
                provider,
                model_ref,
                session_ctx,
                query,
            )
            .await
        {
            Ok(outcome) => {
                self.inner.breaker.record_success();
                if outcome.compacted {
                    info!(
                        tokens_before = outcome.tokens_before,
                        tokens_after = outcome.tokens_after,
                        summary_len = outcome.summary.len(),
                        consecutive_failures = self.inner.breaker.consecutive_failures(),
                        "two-pass compaction succeeded"
                    );
                    let summary = outcome.summary.clone();
                    let boundary = outcome.boundary.clone();
                    *messages = outcome.messages;
                    Ok(Some((summary, boundary)))
                } else {
                    Ok(None)
                }
            }
            Err(err) => {
                let just_opened = self.inner.breaker.record_failure();
                if just_opened {
                    warn!(
                        error = %err,
                        max_consecutive_failures = self.inner.config.max_consecutive_failures,
                        "two-pass compaction failed; circuit breaker opened"
                    );
                } else {
                    warn!(
                        error = %err,
                        consecutive_failures = self.inner.breaker.consecutive_failures(),
                        "two-pass compaction failed; falling back to single-stage"
                    );
                }
                self.inner
                    .compact_if_needed(
                        messages,
                        system_prompt,
                        provider,
                        model_ref,
                        session_ctx,
                        query,
                    )
                    .await
            }
        }
    }

    /// Run the two-pass algorithm. Returns a non-compacted result when the
    /// window is too small or when the split is degenerate.
    async fn compact_conversation_two_pass(
        &self,
        messages: Vec<ChatMessage>,
        system_prompt: &str,
        provider: &ProviderRouter,
        model_ref: &str,
        session_ctx: Option<&SessionContext>,
        query: &str,
    ) -> Result<CompactionResult, RuntimeError> {
        let tokens_before = estimate_total_tokens(&messages, system_prompt);

        let boundary =
            select_compaction_boundary(&messages, self.inner.config.min_messages_to_keep);
        if boundary == 0 {
            return Ok(CompactionResult {
                summary: String::new(),
                messages,
                tokens_before,
                tokens_after: tokens_before,
                compacted: false,
                reattachments: Vec::new(),
                boundary: None,
            });
        }

        let split_fraction = self.inner.config.split_fraction.clamp(0.01, 0.99);
        let split_at =
            ((boundary as f32 * split_fraction) as usize).clamp(1, boundary.saturating_sub(1));
        if split_at == 0 || split_at >= boundary {
            return self
                .inner
                .compact_conversation(
                    messages,
                    system_prompt,
                    provider,
                    model_ref,
                    session_ctx,
                    query,
                )
                .await;
        }

        let summary_model_ref = self
            .inner
            .config
            .summary_model
            .as_deref()
            .unwrap_or(model_ref);

        // Pass 1: summarize the prefix of the compaction window.
        let mut prefix: Vec<ChatMessage> = messages[..split_at].to_vec();
        strip_attachments(
            &mut prefix,
            self.inner.config.strip_images,
            self.inner.config.strip_documents,
        );
        let note1 = generate_summary(
            provider,
            summary_model_ref,
            &prefix,
            self.inner.config.max_summary_tokens,
        )
        .await?;
        if note1.is_empty() {
            return self
                .inner
                .compact_conversation(
                    messages,
                    system_prompt,
                    provider,
                    model_ref,
                    session_ctx,
                    query,
                )
                .await;
        }

        // Pass 2: rewrite the pass-1 note together with the tail.
        let mut tail: Vec<ChatMessage> = messages[split_at..boundary].to_vec();
        strip_attachments(
            &mut tail,
            self.inner.config.strip_images,
            self.inner.config.strip_documents,
        );
        let mut pass2_source = vec![ChatMessage::system(format!(
            "Earlier conversation summary draft:\n\n{note1}"
        ))];
        pass2_source.extend(tail);
        let summary = generate_summary(
            provider,
            summary_model_ref,
            &pass2_source,
            self.inner.config.max_summary_tokens,
        )
        .await?;
        if summary.is_empty() {
            return self
                .inner
                .compact_conversation(
                    messages,
                    system_prompt,
                    provider,
                    model_ref,
                    session_ctx,
                    query,
                )
                .await;
        }

        let compacted = build_compacted_messages(
            &messages,
            boundary,
            &summary,
            self.inner.config.use_prompt_cache,
            session_ctx,
            query,
        )
        .await;
        let tokens_after = estimate_total_tokens(&compacted, system_prompt);

        let boundary_mark = BoundaryMark {
            entry_index: 0,
            timestamp_iso: iso_now(),
            tokens_compacted: tokens_before.saturating_sub(tokens_after),
        };

        Ok(CompactionResult {
            summary,
            messages: compacted,
            tokens_before,
            tokens_after,
            compacted: true,
            reattachments: Vec::new(),
            boundary: Some(boundary_mark),
        })
    }
}

/// Build the replacement message list after a summary has been produced.
///
/// Preserves the original leading system message, injects the summary as a new
/// system message, appends state reattachments, and finally appends the kept
/// tail. This helper is shared between single-stage and two-stage compaction.
async fn build_compacted_messages(
    messages: &[ChatMessage],
    boundary: usize,
    summary: &str,
    use_prompt_cache: bool,
    session_ctx: Option<&SessionContext>,
    query: &str,
) -> Vec<ChatMessage> {
    // Build and inject reattachments so the model retains its capabilities.
    let reattachments = if let Some(ctx) = session_ctx {
        match ctx.build_reattachments(query).await {
            Ok(items) => items,
            Err(err) => {
                warn!(error = %err, "failed to build compaction reattachments");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let mut compacted = Vec::with_capacity(messages.len() - boundary + 2);
    // Preserve the original system prompt if it was the first message.
    if let Some(first) = messages.first() {
        if first.role == ChatRole::System {
            let mut preserved = first.clone();
            if use_prompt_cache {
                preserved.cache_breakpoint = true;
            }
            compacted.push(preserved);
        }
    }

    let mut summary_msg =
        ChatMessage::system(format!("Earlier conversation summary:\n\n{summary}"));
    if use_prompt_cache {
        summary_msg.cache_breakpoint = true;
    }
    compacted.push(summary_msg);

    for item in &reattachments {
        compacted.extend(item.to_messages());
    }

    compacted.extend(messages[boundary..].iter().cloned());
    compacted
}

/// Find the index at which to split `messages` so that the tail can stay
/// verbatim while the head is summarized.
///
/// Returns `0` when compaction is not safe (e.g. too few messages or all
/// messages must be kept to satisfy tool-call invariants).
fn select_compaction_boundary(messages: &[ChatMessage], min_keep: usize) -> usize {
    if messages.len() < min_keep + 1 {
        return 0;
    }

    let first_non_system = messages
        .iter()
        .position(|m| m.role != ChatRole::System)
        .unwrap_or(0);

    let mut keep = min_keep;
    loop {
        if keep > messages.len() - first_non_system {
            return 0;
        }
        let cutoff = messages.len() - keep;

        // Collect tool_call_ids that appear in the kept region.
        let kept_tool_ids: HashSet<String> = messages[cutoff..]
            .iter()
            .filter_map(|m| m.tool_call_id.clone())
            .collect();

        // If a kept tool_result references a tool_call that would be summarized,
        // we must extend the kept region to include that assistant message.
        let mut bad = false;
        for msg in messages[first_non_system..cutoff].iter().rev() {
            if let Some(tool_calls) = &msg.tool_calls {
                if tool_calls.iter().any(|tc| kept_tool_ids.contains(&tc.id)) {
                    bad = true;
                    break;
                }
            }
        }

        if !bad {
            // Also avoid cutting immediately after a user message if the next
            // message is an assistant/tool pair; this keeps natural turns intact.
            return cutoff.max(first_non_system + 1);
        }

        keep += 1;
    }
}

async fn generate_summary(
    provider: &ProviderRouter,
    model_ref: &str,
    source: &[ChatMessage],
    max_tokens: usize,
) -> Result<String, RuntimeError> {
    let mut summary_messages = vec![ChatMessage::system(SUMMARY_SYSTEM_PROMPT)];
    summary_messages.extend(source.iter().cloned());

    let mut req = ChatRequest::new(model_ref, summary_messages);
    req.max_tokens = Some(max_tokens as u32);

    let mut stream = provider.chat(model_ref, req).await?;
    let mut summary = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        summary.push_str(&chunk.delta);
        if matches!(
            chunk.finish_reason,
            Some(FinishReason::Stop) | Some(FinishReason::Length)
        ) {
            break;
        }
    }

    Ok(summary.trim().to_string())
}

/// Replace attachment payloads in message content with short placeholders before
/// they are sent to the summary model.
fn strip_attachments(messages: &mut [ChatMessage], strip_images: bool, strip_documents: bool) {
    if !strip_images && !strip_documents {
        return;
    }
    for msg in messages.iter_mut() {
        msg.content = strip_attachment_markers(&msg.content, strip_images, strip_documents);
    }
}

fn strip_attachment_markers(content: &str, strip_images: bool, strip_documents: bool) -> String {
    let mut result = content.to_string();
    if strip_images {
        result = replace_markdown_images(&result);
    }
    if strip_images || strip_documents {
        result = replace_data_uris(&result, strip_images, strip_documents);
    }
    result
}

/// Replace Markdown image syntax `![alt](url)` with `[image: alt]`.
fn replace_markdown_images(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find("![") {
        result.push_str(&rest[..start]);
        rest = &rest[start..];

        if let Some(mid) = rest.find("](") {
            let alt = &rest[2..mid];
            if let Some(close) = rest[mid + 2..].find(')') {
                result.push_str("[image: ");
                result.push_str(alt);
                result.push(']');
                rest = &rest[mid + 2 + close + 1..];
                continue;
            }
        }

        // Malformed image marker; copy the two characters and continue.
        result.push_str("![");
        rest = &rest[2..];
    }

    result.push_str(rest);
    result
}

/// Replace data URIs with short placeholders.
///
/// Image MIME types become `[image]`; all other MIME types become `[attachment]`.
fn replace_data_uris(content: &str, strip_images: bool, strip_documents: bool) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find("data:") {
        result.push_str(&rest[..start]);
        rest = &rest[start..];

        if let Some(base64_rel) = rest.find(";base64,") {
            let mime = &rest[5..base64_rel];
            let after_base64 = &rest[base64_rel + 8..];
            let data_len = after_base64
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '/' && c != '=')
                .unwrap_or(after_base64.len());
            let full_end = base64_rel + 8 + data_len;

            if mime.starts_with("image/") {
                if strip_images {
                    result.push_str("[image]");
                } else {
                    result.push_str(&rest[..full_end]);
                }
            } else if strip_documents {
                result.push_str("[attachment]");
            } else {
                result.push_str(&rest[..full_end]);
            }
            rest = &rest[full_end..];
        } else {
            // Not a valid data URI; copy the prefix and continue.
            result.push('d');
            rest = &rest[1..];
        }
    }

    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use legion_provider::provider::Provider;
    use legion_provider::router::ProviderRouter;
    use legion_provider::types::{
        ChatChunk, ChatStream, EmbedRequest, Embedding, FunctionCall, ModelInfo, ProviderError,
        ToolCall,
    };

    fn config() -> CompactionConfig {
        CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.8,
            min_messages_to_keep: 2,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        }
    }

    fn buffer_config() -> CompactionConfig {
        CompactionConfig {
            context_window: 100_000,
            threshold_ratio: 0.8,
            min_messages_to_keep: 2,
            max_summary_tokens: 256,
            buffer_tokens: 13_000,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        }
    }

    #[test]
    fn circuit_breaker_opens_after_max_failures() {
        let breaker = CircuitBreaker::new(3);
        assert!(breaker.allow());
        assert!(!breaker.record_failure());
        assert!(breaker.allow());
        assert!(!breaker.record_failure());
        assert!(breaker.allow());
        assert!(breaker.record_failure());
        assert!(!breaker.allow());
    }

    #[test]
    fn circuit_breaker_resets_on_success() {
        let breaker = CircuitBreaker::new(3);
        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.consecutive_failures(), 2);
        breaker.record_success();
        assert_eq!(breaker.consecutive_failures(), 0);
        assert!(breaker.allow());
    }

    #[test]
    fn should_compact_when_above_threshold() {
        // Produce enough tokens to exceed the 800-token threshold (1000 * 0.8)
        // and include enough messages to satisfy the minimum-keep guard.
        let big_content = "word ".repeat(1500);
        let cfg = CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.8,
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        };
        let messages = vec![
            ChatMessage::system("you are helpful"),
            ChatMessage::user(&big_content),
            ChatMessage::assistant("ack"),
            ChatMessage::user("next"),
        ];
        let compactor = Compactor::new(cfg);
        assert!(compactor.should_compact(&messages, "", "provider/model"));
    }

    #[test]
    fn should_not_compact_when_below_threshold() {
        let messages = vec![ChatMessage::user("hi")];
        let compactor = Compactor::new(config());
        assert!(!compactor.should_compact(&messages, "", "provider/model"));
    }

    #[test]
    fn buffer_triggers_before_ratio_threshold() {
        // context_window=100_000, buffer=13_000 => trigger at >= 87_000 tokens.
        // ratio threshold would be 80_000, so buffer triggers first.
        let big_content = "word ".repeat(90_000); // well over 87k tokens
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(&big_content),
            ChatMessage::assistant("ack"),
            ChatMessage::user("next"),
        ];
        let compactor = Compactor::new(buffer_config());
        assert!(compactor.should_compact(&messages, "", "provider/model"));
    }

    #[test]
    fn buffer_zero_falls_back_to_ratio() {
        let big_content = "word ".repeat(1_500); // ~ old 800-token threshold
        let cfg = CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.8,
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        };
        let messages = vec![
            ChatMessage::system("you are helpful"),
            ChatMessage::user(&big_content),
            ChatMessage::assistant("ack"),
            ChatMessage::user("next"),
        ];
        let compactor = Compactor::new(cfg);
        assert!(compactor.should_compact(&messages, "", "provider/model"));
    }

    #[test]
    fn effective_window_uses_global_fallback() {
        let compactor = Compactor::new(config());
        assert_eq!(compactor.effective_context_window("provider/model"), 1_000,);
    }

    #[test]
    fn effective_window_suffix_overrides_table_and_global() {
        // Global = 1_000 (from `config()`), table = 2_000, suffix = 1_000_000.
        // Suffix must win.
        let mut cfg = config();
        cfg.context_windows
            .insert("provider/model".to_string(), 2_000);
        let compactor = Compactor::new(cfg);
        assert_eq!(
            compactor.effective_context_window("provider/model[1m]"),
            1_000_000,
        );
    }

    #[test]
    fn effective_window_table_overrides_global() {
        let mut cfg = config();
        cfg.context_windows
            .insert("minimax/MiniMax-M3".to_string(), 1_000_000);
        let compactor = Compactor::new(cfg);
        assert_eq!(
            compactor.effective_context_window("minimax/MiniMax-M3"),
            1_000_000,
        );
        // An unlisted model falls back to the global value.
        assert_eq!(compactor.effective_context_window("provider/other"), 1_000,);
    }

    #[test]
    fn effective_window_ignores_malformed_suffix() {
        let compactor = Compactor::new(config());
        // `[abc]` is not a valid window, so the global value is used.
        assert_eq!(
            compactor.effective_context_window("provider/model[abc]"),
            1_000,
        );
    }

    #[test]
    fn should_compact_uses_suffix_window() {
        // Global window 1_000 / ratio 0.8 => legacy threshold 800, which the
        // content below would exceed. But a `[1000000]` suffix widens the
        // window so compaction must NOT trigger.
        let big_content = "word ".repeat(1_500);
        let messages = vec![
            ChatMessage::system("you are helpful"),
            ChatMessage::user(&big_content),
            ChatMessage::assistant("ack"),
            ChatMessage::user("next"),
        ];
        let compactor = Compactor::new(config());
        assert!(!compactor.should_compact(&messages, "", "provider/model[1000000]",));
    }

    #[test]
    fn select_boundary_keeps_recent_messages() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user("recent"),
            ChatMessage::assistant("b"),
        ];
        let boundary = select_compaction_boundary(&messages, 2);
        assert_eq!(boundary, 3);
    }

    #[test]
    fn select_boundary_preserves_tool_call_invariant() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: "".to_string(),
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call-1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "read".to_string(),
                        arguments: r#"{"path":"x"}"#.to_string(),
                    },
                }]),
                tool_call_id: None,
                cache_breakpoint: false,
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "result".to_string(),
                name: None,
                tool_calls: None,
                tool_call_id: Some("call-1".to_string()),
                cache_breakpoint: false,
            },
            ChatMessage::user("recent"),
            ChatMessage::assistant("recent reply"),
        ];
        // Force the tool_result into the kept window (min_keep=3). Its matching
        // assistant tool_use must then be kept as well.
        let boundary = select_compaction_boundary(&messages, 3);
        assert_eq!(boundary, 1);
    }

    #[test]
    fn strip_image_data_uri() {
        let input = "Here is an image: data:image/png;base64,AAAAbase64== and text after";
        let out = strip_attachment_markers(input, true, true);
        assert_eq!(out, "Here is an image: [image] and text after");
    }

    #[test]
    fn strip_document_data_uri() {
        let input = "See file: data:application/pdf;base64,BBBBpdf== end";
        let out = strip_attachment_markers(input, true, true);
        assert_eq!(out, "See file: [attachment] end");
    }

    #[test]
    fn strip_markdown_image() {
        let input = "Look ![screenshot](data:image/png;base64,AAAA==) here";
        let out = strip_attachment_markers(input, true, true);
        assert_eq!(out, "Look [image: screenshot] here");
    }

    #[test]
    fn strip_disabled_leaves_content_intact() {
        let input = "data:image/png;base64,AAAA== ![alt](url)";
        let out = strip_attachment_markers(input, false, false);
        assert_eq!(out, input);
    }

    #[test]
    fn strip_images_only_leaves_documents() {
        let input = "img: data:image/png;base64,AAAA== doc: data:application/pdf;base64,BBBB==";
        let out = strip_attachment_markers(input, true, false);
        assert_eq!(out, "img: [image] doc: data:application/pdf;base64,BBBB==");
    }

    #[test]
    fn strip_documents_only_leaves_images() {
        let input = "img: data:image/png;base64,AAAA== doc: data:application/pdf;base64,BBBB==";
        let out = strip_attachment_markers(input, false, true);
        assert_eq!(out, "img: data:image/png;base64,AAAA== doc: [attachment]");
    }

    #[test]
    fn strip_preserves_non_attachment_text() {
        let input = "Regular text with data: not a URI and a date: 2024-01-01";
        let out = strip_attachment_markers(input, true, true);
        assert_eq!(out, input);
    }

    struct SummaryProvider;

    #[async_trait]
    impl Provider for SummaryProvider {
        fn id(&self) -> &str {
            "summary"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                index: 0,
                delta: "summary text".to_string(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            })])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Provider whose summary call always fails (circuit-breaker tests).
    struct FailingSummaryProvider {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for FailingSummaryProvider {
        fn id(&self) -> &str {
            "fail"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(ProviderError::StreamAborted("boom".to_string()))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Provider that returns an empty summary (empty-summary guard tests).
    struct EmptySummaryProvider;

    #[async_trait]
    impl Provider for EmptySummaryProvider {
        fn id(&self) -> &str {
            "empty"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                index: 0,
                delta: String::new(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            })])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn compact_conversation_replaces_old_messages_with_summary() {
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(SummaryProvider));

        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old message"),
            ChatMessage::assistant("old reply"),
            ChatMessage::user("recent"),
            ChatMessage::assistant("recent reply"),
        ];

        let compactor = Compactor::new(CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.0, // force compaction
            min_messages_to_keep: 2,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        });

        let result = compactor
            .compact_conversation(messages, "sys", &router, "summary/gpt", None, "")
            .await
            .unwrap();

        assert!(result.compacted);
        assert_eq!(result.messages.len(), 4); // sys + summary + recent pair
        assert!(result.messages[0].content.contains("sys"));
        assert!(result.messages[1].content.contains("summary text"));
        assert_eq!(result.messages[2].content, "recent");
        assert_eq!(result.messages[3].content, "recent reply");
        assert!(result.tokens_before > result.tokens_after);
    }

    #[tokio::test]
    async fn compact_if_needed_skips_when_breaker_open() {
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(SummaryProvider));

        let cfg = CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.0,
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 1,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        };
        let compactor = Compactor::new(cfg);
        compactor.breaker.record_failure();
        assert!(!compactor.breaker.allow());

        let mut messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("recent"),
        ];

        let summary = compactor
            .compact_if_needed(&mut messages, "", &router, "summary/gpt", None, "")
            .await
            .unwrap();
        assert!(summary.is_none());
    }

    struct EchoSummaryProvider;

    #[async_trait]
    impl Provider for EchoSummaryProvider {
        fn id(&self) -> &str {
            "echo-summary"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            // Echo all user/assistant messages sent as the summary source.
            let content = req
                .messages
                .iter()
                .skip(1)
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                index: 0,
                delta: content,
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            })])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn compact_if_needed_strips_attachments_before_summary() {
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(EchoSummaryProvider));

        let cfg = CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.0,
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        };
        let compactor = Compactor::new(cfg);

        let mut messages = vec![
            ChatMessage::user("old"),
            ChatMessage::user(format!(
                "Look at this: data:image/png;base64,{}",
                "A".repeat(200)
            )),
            ChatMessage::assistant("recent reply"),
        ];

        compactor
            .compact_if_needed(&mut messages, "", &router, "echo-summary/gpt", None, "")
            .await
            .unwrap();

        // The summary message should contain the placeholder, not the base64 blob.
        let summary_msg = messages
            .iter()
            .find(|m| m.role == ChatRole::System && m.content.contains("Earlier conversation"))
            .expect("summary message should exist");
        assert!(summary_msg.content.contains("[image]"));
        assert!(!summary_msg.content.contains("data:image/png"));
    }

    #[tokio::test]
    async fn compact_if_needed_records_failures_and_opens_breaker() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(FailingSummaryProvider {
            calls: calls.clone(),
        }));

        let cfg = CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.0, // force compaction
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 2,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        };
        let compactor = Compactor::new(cfg);

        let mut messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("recent"),
        ];

        // First failure propagates and records one strike; breaker stays closed.
        let err = compactor
            .compact_if_needed(&mut messages, "", &router, "fail/gpt", None, "")
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Provider(_)));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(compactor.breaker.allow());

        // Second consecutive failure opens the breaker.
        let err = compactor
            .compact_if_needed(&mut messages, "", &router, "fail/gpt", None, "")
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Provider(_)));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(!compactor.breaker.allow());

        // With the breaker open, compaction is skipped without invoking the
        // provider again.
        let outcome = compactor
            .compact_if_needed(&mut messages, "", &router, "fail/gpt", None, "")
            .await
            .unwrap();
        assert!(outcome.is_none());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "provider must not be called while the breaker is open"
        );
    }

    #[test]
    fn select_boundary_returns_zero_when_too_few_messages() {
        // len < min_keep + 1: nothing beyond the keep region to summarize.
        let messages = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
        assert_eq!(select_compaction_boundary(&messages, 2), 0);

        // len == min_keep is also too few.
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("hello")];
        assert_eq!(select_compaction_boundary(&messages, 2), 0);
    }

    #[test]
    fn select_boundary_returns_zero_when_keep_region_covers_all_non_system() {
        // min_keep spans every non-system message, so there is nothing safe to
        // summarize and the boundary scan must give up with 0.
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::system("more sys"),
            ChatMessage::user("hi"),
        ];
        assert_eq!(select_compaction_boundary(&messages, 2), 0);
    }

    #[tokio::test]
    async fn compact_conversation_empty_summary_reports_not_compacted() {
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(EmptySummaryProvider));

        let compactor = Compactor::new(CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.0, // force compaction
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            ..Default::default()
        });

        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("recent"),
        ];
        let result = compactor
            .compact_conversation(messages.clone(), "", &router, "empty/gpt", None, "")
            .await
            .unwrap();

        assert!(!result.compacted);
        assert!(result.summary.is_empty());
        assert_eq!(result.messages, messages);
        assert_eq!(result.tokens_after, result.tokens_before);
        assert!(result.boundary.is_none());
    }

    #[tokio::test]
    async fn compaction_marks_cache_breakpoints_when_enabled() {
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(SummaryProvider));

        let compactor = Compactor::new(CompactionConfig {
            context_window: 1_000,
            threshold_ratio: 0.0, // force compaction
            min_messages_to_keep: 2,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: true,
            ..Default::default()
        });

        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old message"),
            ChatMessage::assistant("old reply"),
            ChatMessage::user("recent"),
            ChatMessage::assistant("recent reply"),
        ];

        let result = compactor
            .compact_conversation(messages, "sys", &router, "summary/gpt", None, "")
            .await
            .unwrap();

        assert!(result.compacted);
        assert!(
            result.messages[0].cache_breakpoint,
            "preserved system message should be a cache breakpoint: {:?}",
            result.messages[0]
        );
        assert_eq!(result.messages[0].content, "sys");
        assert!(
            result.messages[1].cache_breakpoint,
            "summary message should be a cache breakpoint: {:?}",
            result.messages[1]
        );
        assert!(
            result.messages[1]
                .content
                .contains("Earlier conversation summary")
        );
    }

    /// Provider that distinguishes pass 1 from pass 2 of two-stage compaction:
    /// pass 2 sees the "Earlier conversation summary draft" system message.
    struct TwoPassSummaryProvider;

    #[async_trait]
    impl Provider for TwoPassSummaryProvider {
        fn id(&self) -> &str {
            "two-pass-summary"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let is_pass2 = req
                .messages
                .iter()
                .skip(1)
                .any(|m| m.content.contains("Earlier conversation summary draft"));
            let delta = if is_pass2 { "FINAL" } else { "NOTE1" }.to_string();
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                index: 0,
                delta,
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            })])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn two_pass_compactor_runs_two_summary_stages() {
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(TwoPassSummaryProvider));

        let cfg = CompactionConfig {
            context_window: 1_000,
            context_windows: std::collections::BTreeMap::new(),
            threshold_ratio: 0.0, // force compaction
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            two_pass_enabled: true,
            split_fraction: 0.5,
            prefire_lead_percent: 10,
        };
        let compactor = TwoPassCompactor::new(cfg);

        // Boundary = 4 (keep 1), split_at = 2 with split_fraction 0.5.
        let mut messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("two"),
            ChatMessage::user("three"),
            ChatMessage::assistant("four"),
            ChatMessage::user("recent"),
        ];

        let result = compactor
            .compact_if_needed(&mut messages, "", &router, "two-pass-summary/gpt", None, "")
            .await
            .unwrap();

        assert!(result.is_some(), "two-pass compaction should occur");
        let (summary, _) = result.unwrap();
        assert_eq!(summary, "FINAL", "pass 2 should produce the final summary");
        assert_eq!(
            messages.len(),
            2,
            "compacted list is summary + kept tail (1 message)"
        );
        assert!(
            messages[0].content.contains("Earlier conversation summary"),
            "first message should be the final summary"
        );
        assert_eq!(messages[1].content, "recent");
    }

    #[tokio::test]
    async fn two_pass_compactor_falls_back_when_disabled() {
        let mut router = ProviderRouter::new();
        router.register_provider(std::sync::Arc::new(TwoPassSummaryProvider));

        let cfg = CompactionConfig {
            context_window: 1_000,
            context_windows: std::collections::BTreeMap::new(),
            threshold_ratio: 0.0,
            min_messages_to_keep: 1,
            max_summary_tokens: 256,
            buffer_tokens: 0,
            max_consecutive_failures: 3,
            strip_images: true,
            strip_documents: true,
            summary_model: None,
            use_prompt_cache: false,
            two_pass_enabled: false,
            split_fraction: 0.5,
            prefire_lead_percent: 10,
        };
        let compactor = TwoPassCompactor::new(cfg);

        let mut messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("two"),
            ChatMessage::user("recent"),
        ];

        let result = compactor
            .compact_if_needed(&mut messages, "", &router, "two-pass-summary/gpt", None, "")
            .await
            .unwrap();

        assert!(result.is_some());
        // Single-stage path calls the provider once and gets NOTE1.
        assert_eq!(result.unwrap().0, "NOTE1");
    }
}
