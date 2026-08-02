//! Shared helpers for the small "LLM call → accumulate stream → parse a JSON
//! array" flows used by selectors and extractors. Every failure mode is logged
//! and reported as `None`/empty so callers can fall back instead of failing.

use std::time::Duration;

use futures::StreamExt;
use legion_provider::router::ProviderRouter;
use legion_provider::types::ChatRequest;

/// Run `router.chat` under `timeout` and accumulate the streamed deltas into a
/// single string. Returns `None` — with a warning logged — on timeout, call
/// failure, or mid-stream error.
pub async fn chat_text_with_timeout(
    router: &ProviderRouter,
    model_ref: &str,
    req: ChatRequest,
    timeout: Duration,
) -> Option<String> {
    match tokio::time::timeout(timeout, router.chat(model_ref, req)).await {
        Ok(Ok(mut stream)) => {
            let mut text = String::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => text.push_str(&c.delta),
                    Err(e) => {
                        tracing::warn!(error = %e, "LLM stream error");
                        return None;
                    }
                }
            }
            Some(text)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "LLM call failed");
            None
        }
        Err(_) => {
            tracing::warn!("LLM call timed out");
            None
        }
    }
}

/// Extract the first JSON array in `text` — the slice from the first `[` to
/// the last `]` — and deserialize its elements. Returns `None` when no array
/// is present or the slice does not parse.
pub fn extract_json_array<T: serde::de::DeserializeOwned>(text: &str) -> Option<Vec<T>> {
    let (Some(s), Some(e)) = (text.find('['), text.rfind(']')) else {
        return None;
    };
    if e < s {
        return None;
    }
    serde_json::from_str(&text[s..=e]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_array_pulls_first_array() {
        assert_eq!(
            extract_json_array::<usize>("here [3, 1, 2] done"),
            Some(vec![3, 1, 2])
        );
        assert_eq!(
            extract_json_array::<String>("noise [\"a\", \"b\"]"),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn extract_json_array_rejects_garbage() {
        assert_eq!(extract_json_array::<usize>("nothing"), None);
        assert_eq!(extract_json_array::<usize>("[1, 2"), None);
        assert_eq!(extract_json_array::<usize>("] ["), None);
        assert_eq!(extract_json_array::<usize>("[\"not a number\"]"), None);
    }
}
