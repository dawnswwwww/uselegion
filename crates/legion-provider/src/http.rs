//! Shared HTTP transport helpers for the JSON chat providers: client
//! construction, status-code mapping, and the SSE → `ChatStream` pipeline.

use crate::types::{ChatChunk, ChatStream, ProviderError};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use std::collections::HashMap;
use std::time::Duration;

/// Build a header value, mapping invalid characters to an auth error (the
/// value almost always carries an API key).
pub(crate) fn header_value(value: &str) -> Result<reqwest::header::HeaderValue, ProviderError> {
    reqwest::header::HeaderValue::from_str(value)
        .map_err(|e| ProviderError::InvalidAuth(e.to_string()))
}

/// Build a reqwest client with the given default headers plus a JSON content
/// type, as used by every HTTP chat provider.
pub(crate) fn json_client(
    mut headers: reqwest::header::HeaderMap,
) -> Result<reqwest::Client, ProviderError> {
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .build()?)
}

/// POST `body` as JSON to `url`, applying the provider's extra headers.
pub(crate) fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
    extra_headers: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    let mut builder = client.post(url).json(&body);
    for (k, v) in extra_headers {
        builder = builder.header(k, v);
    }
    builder
}

/// Map a non-success HTTP response to a `StreamAborted` error carrying the
/// status and body, returning the response unchanged on success. When
/// `prompt_too_long` is set, context-window overflow bodies map to
/// `ProviderError::PromptTooLong` instead.
pub(crate) async fn check_status(
    response: reqwest::Response,
    prompt_too_long: bool,
) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    if prompt_too_long && is_prompt_too_long(&text) {
        return Err(ProviderError::PromptTooLong);
    }
    Err(ProviderError::StreamAborted(format!(
        "HTTP {status}: {text}"
    )))
}

/// Detect context-window overflow error bodies. The keyword list is the union
/// of what the OpenAI- and Anthropic-compatible endpoints emit.
fn is_prompt_too_long(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("prompt is too long")
        || lower.contains("context overflow")
        || lower.contains("context_overflow")
        || lower.contains("context length")
        || lower.contains("max tokens")
}

/// Parse one SSE `data:` payload as JSON. Malformed payloads map to
/// `ProviderError::JsonParse` (the payload is JSON, not an SSE framing
/// problem), consistent across all providers.
pub(crate) fn parse_sse_json<T: serde::de::DeserializeOwned>(
    data: &str,
) -> Result<T, ProviderError> {
    serde_json::from_str(data).map_err(ProviderError::JsonParse)
}

/// Turn an SSE response body into a `ChatStream`.
///
/// `handle` receives the raw `data:` payload and returns `Some` to emit a
/// chunk (or an error) and `None` to skip the event. SSE framing errors
/// surface as `ProviderError::SseParse`.
pub(crate) fn sse_stream<S, F>(response: reqwest::Response, state: S, handle: F) -> ChatStream
where
    S: Send + 'static,
    F: FnMut(&mut S, &str) -> Option<Result<ChatChunk, ProviderError>> + Send + 'static,
{
    let stream = response
        .bytes_stream()
        .eventsource()
        // scan terminates the stream on None, so always yield Some(result)
        // and let filter_map drop the inner None values.
        .scan((state, handle), |(state, handle), event| {
            let result = match event {
                Ok(event) => handle(state, &event.data),
                Err(err) => Some(Err(ProviderError::SseParse(err.to_string()))),
            };
            futures::future::ready(Some(result))
        })
        .filter_map(futures::future::ready);
    Box::pin(stream)
}

/// Wrap a chat stream with a per-chunk idle timeout: if no item arrives
/// within `limit`, the stream yields one `ProviderError::Timeout` and ends.
///
/// Applied by the router, the only layer that knows the configured
/// `timeout_seconds` — the per-request timeout otherwise only covers stream
/// establishment. Reusing the same budget as an idle limit is a deliberate
/// approximation: it bounds stalls between chunks, not total stream duration.
pub(crate) fn with_idle_timeout(
    stream: ChatStream,
    limit: Option<Duration>,
    provider_id: &str,
) -> ChatStream {
    let Some(limit) = limit else {
        return stream;
    };
    let provider = provider_id.to_string();
    Box::pin(futures::stream::unfold(
        (stream, false),
        move |(mut inner, mut expired)| {
            let provider = provider.clone();
            async move {
                if expired {
                    return None;
                }
                match tokio::time::timeout(limit, inner.next()).await {
                    Ok(Some(item)) => Some((item, (inner, expired))),
                    Ok(None) => None,
                    Err(_) => {
                        expired = true;
                        Some((
                            Err(ProviderError::Timeout(format!(
                                "provider '{provider}' exceeded timeout of {}ms",
                                limit.as_millis()
                            ))),
                            (inner, expired),
                        ))
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_prompt_too_long_from_error_body() {
        assert!(is_prompt_too_long(
            r#"{"error":{"code":"context_length_exceeded","message":"..."}}"#
        ));
        assert!(is_prompt_too_long(
            "This model's maximum context length is 8192 tokens"
        ));
        assert!(is_prompt_too_long(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#
        ));
        assert!(is_prompt_too_long(
            r#"{"type":"error","error":{"type":"context_overflow","message":"..."}}"#
        ));
        assert!(!is_prompt_too_long("invalid_api_key"));
    }
}
