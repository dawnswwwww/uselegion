use crate::auth::AuthProfile;
use crate::http;
use crate::provider::Provider;
use crate::types::{
    ChatChunk, ChatRequest, ChatStream, EmbedRequest, Embedding, FinishReason, GeneratedImage,
    ImageRequest, ImageResponse, ModelInfo, ProviderError, SpeechRequest, SpeechResponse,
    ToolCallAccumulator, role_str,
};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// An OpenAI-compatible provider. Works for OpenAI, MiniMax OpenAI endpoint,
/// OpenRouter, and any other provider exposing `/chat/completions` and
/// `/embeddings`.
pub struct OpenAiProvider {
    id: String,
    base_url: String,
    client: reqwest::Client,
    models: Vec<ModelInfo>,
    extra_headers: HashMap<String, String>,
}

impl OpenAiProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: Option<String>,
        auth: AuthProfile,
    ) -> Result<Self, ProviderError> {
        let key = auth
            .api_key_value()
            .ok_or_else(|| {
                ProviderError::InvalidAuth("OpenAI provider requires an API key".to_string())
            })?
            .to_string();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            http::header_value(&format!("Bearer {key}"))?,
        );

        let client = http::json_client(headers)?;

        Ok(Self {
            id: id.into(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
            client,
            models: Vec::new(),
            extra_headers: HashMap::new(),
        })
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        self.models.push(ModelInfo::new(&model, &self.id));
        self
    }

    pub fn with_extra_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.extra_headers = headers;
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn embed_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    fn image_url(&self) -> String {
        format!("{}/images/generations", self.base_url.trim_end_matches('/'))
    }

    fn speech_url(&self) -> String {
        format!("{}/audio/speech", self.base_url.trim_end_matches('/'))
    }

    fn build_body(&self, req: &ChatRequest) -> serde_json::Value {
        let mut messages: Vec<serde_json::Value> = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            let mut obj = serde_json::json!({
                "role": role_str(&m.role),
                "content": m.content,
            });
            if let Some(name) = &m.name {
                obj["name"] = serde_json::Value::String(name.clone());
            }
            if let Some(tool_calls) = &m.tool_calls {
                obj["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_default();
            }
            if let Some(id) = &m.tool_call_id {
                obj["tool_call_id"] = serde_json::Value::String(id.clone());
            }
            messages.push(obj);
        }

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
        });

        if let Some(t) = req.temperature {
            body["temperature"] = serde_json::Value::from(t);
        }
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::Value::from(max);
        }
        if let Some(p) = req.top_p {
            body["top_p"] = serde_json::Value::from(p);
        }
        if let Some(stop) = &req.stop {
            body["stop"] = serde_json::to_value(stop).unwrap_or_default();
        }
        if let Some(tools) = &req.tools {
            let openai_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(openai_tools);
        }
        req.apply_extra_to(&mut body);

        body
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn supported_models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let body = self.build_body(&req);
        tracing::debug!(url = %self.chat_url(), body = %serde_json::to_string(&body).unwrap_or_default(), "OpenAI chat request");
        let response = http::check_status(
            http::post_json(&self.client, &self.chat_url(), body, &self.extra_headers)
                .send()
                .await?,
            true,
        )
        .await?;

        Ok(http::sse_stream(
            response,
            ToolCallAccumulator::default(),
            |state, data| {
                if data == "[DONE]" {
                    return Some(Ok(ChatChunk {
                        finish_reason: Some(FinishReason::Stop),
                        ..Default::default()
                    }));
                }
                Some(
                    http::parse_sse_json::<OpenAiCompletionChunk>(data)
                        .and_then(|chunk| chunk.into_chat_chunk(state)),
                )
            },
        ))
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
        });

        let response = http::post_json(&self.client, &self.embed_url(), body, &self.extra_headers)
            .send()
            .await?
            .error_for_status()?;

        let payload: OpenAiEmbeddingResponse = response.json().await?;
        Ok(payload
            .data
            .into_iter()
            .map(|d| Embedding {
                index: d.index,
                embedding: d.embedding,
            })
            .collect())
    }

    async fn generate_image(&self, req: ImageRequest) -> Result<ImageResponse, ProviderError> {
        let mut body = serde_json::json!({
            "model": req.model,
            "prompt": req.prompt,
        });
        if let Some(size) = &req.size {
            body["size"] = serde_json::Value::String(size.clone());
        }
        if let Some(n) = req.n {
            body["n"] = serde_json::Value::from(n);
        }

        let response = http::check_status(
            http::post_json(&self.client, &self.image_url(), body, &self.extra_headers)
                .send()
                .await?,
            false,
        )
        .await?;

        let payload: OpenAiImageResponse = response.json().await?;
        Ok(ImageResponse {
            images: payload
                .data
                .into_iter()
                .map(|d| GeneratedImage {
                    url: d.url,
                    b64_json: d.b64_json,
                })
                .collect(),
        })
    }

    async fn synthesize_speech(&self, req: SpeechRequest) -> Result<SpeechResponse, ProviderError> {
        let format = req.format.unwrap_or_else(|| "mp3".to_string());
        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
            "voice": req.voice.unwrap_or_else(|| "alloy".to_string()),
            "response_format": format,
        });

        let response = http::check_status(
            http::post_json(&self.client, &self.speech_url(), body, &self.extra_headers)
                .send()
                .await?,
            false,
        )
        .await?;

        let audio = response.bytes().await?.to_vec();
        Ok(SpeechResponse { audio, format })
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletionChunk {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiChoice {
    index: usize,
    #[serde(default)]
    delta: OpenAiDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCallDelta>>,
}

/// A partial tool-call delta as it appears in a single streaming chunk.
#[derive(Debug, Deserialize, Default)]
struct OpenAiToolCallDelta {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

/// Apply a batch of streaming tool-call deltas to the shared accumulator.
///
/// OpenAI spreads a tool call across multiple chunks: the first chunk usually
/// contains `id`, `type`, and `function.name`, while subsequent chunks only
/// append `function.arguments`.
fn apply_tool_deltas(state: &mut ToolCallAccumulator, deltas: Vec<OpenAiToolCallDelta>) {
    for d in deltas {
        let entry = state.entry(d.index);
        if let Some(id) = d.id {
            entry.id = id;
        }
        if let Some(kind) = d.kind {
            entry.kind = kind;
        }
        if let Some(function) = d.function {
            if let Some(name) = function.name {
                entry.name = name;
            }
            if let Some(arguments) = function.arguments {
                entry.arguments.push_str(&arguments);
            }
        }
    }
}

impl OpenAiCompletionChunk {
    fn into_chat_chunk(self, state: &mut ToolCallAccumulator) -> Result<ChatChunk, ProviderError> {
        let choice = self.choices.into_iter().next().unwrap_or_default();
        let finish_reason = choice.finish_reason.and_then(|r| match r.as_str() {
            "stop" => Some(FinishReason::Stop),
            "length" => Some(FinishReason::Length),
            "tool_calls" => Some(FinishReason::ToolCalls),
            "content_filter" => Some(FinishReason::ContentFilter),
            _ => None,
        });

        if let Some(deltas) = choice.delta.tool_calls {
            apply_tool_deltas(state, deltas);
        }

        let tool_calls = if finish_reason == Some(FinishReason::ToolCalls) {
            let calls = std::mem::take(state).into_complete_calls();
            if calls.is_empty() {
                return Err(ProviderError::StreamAborted(
                    "tool_calls finish reason with no complete tool calls".to_string(),
                ));
            }
            Some(calls)
        } else {
            None
        };

        Ok(ChatChunk {
            index: choice.index,
            delta: choice.delta.content.unwrap_or_default(),
            finish_reason,
            tool_calls,
        })
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiImageResponse {
    data: Vec<OpenAiImageData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiImageData {
    url: Option<String>,
    b64_json: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn build_body_sets_stream_true() {
        let provider = OpenAiProvider::new("openai", None, AuthProfile::api_key("sk-test"))
            .unwrap()
            .with_model("gpt-4");

        let req = ChatRequest::new("gpt-4", vec![ChatMessage::user("hello")]);
        let body = provider.build_body(&req);
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "gpt-4");
        assert!(body["messages"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn build_body_includes_extra_params() {
        let provider =
            OpenAiProvider::new("openai", None, AuthProfile::api_key("sk-test")).unwrap();
        let mut req = ChatRequest::new("gpt-4", vec![ChatMessage::user("hello")]);
        req.extra
            .insert("custom_key".to_string(), serde_json::json!("custom_value"));

        let body = provider.build_body(&req);
        assert_eq!(body["custom_key"], "custom_value");
    }

    #[test]
    fn accumulates_streaming_tool_call_deltas() {
        let mut state = ToolCallAccumulator::default();

        let chunk1: OpenAiCompletionChunk = serde_json::from_str(
            r#"{
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": null,
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "exec", "arguments": "" }
                        }]
                    },
                    "finish_reason": null
                }]
            }"#,
        )
        .unwrap();
        let result1 = chunk1.into_chat_chunk(&mut state).unwrap();
        assert!(result1.tool_calls.is_none());

        let chunk2: OpenAiCompletionChunk = serde_json::from_str(
            r#"{
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": null,
                        "tool_calls": [{
                            "index": 0,
                            "function": { "arguments": "{\"cmd\":\"ls\"}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .unwrap();
        let result2 = chunk2.into_chat_chunk(&mut state).unwrap();
        assert_eq!(result2.finish_reason, Some(FinishReason::ToolCalls));
        let calls = result2.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"cmd":"ls"}"#);
    }

    #[tokio::test]
    async fn generate_image_parses_url_and_b64_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "url": "https://example.com/cat.png" },
                    { "b64_json": "aGVsbG8=" }
                ]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(
            "openai",
            Some(server.uri()),
            AuthProfile::api_key("sk-test"),
        )
        .unwrap();
        let req = ImageRequest {
            model: "dall-e-3".to_string(),
            prompt: "a cat".to_string(),
            size: Some("1024x1024".to_string()),
            n: None,
        };
        let resp = provider.generate_image(req).await.unwrap();
        assert_eq!(resp.images.len(), 2);
        assert_eq!(
            resp.images[0].url.as_deref(),
            Some("https://example.com/cat.png")
        );
        assert!(resp.images[0].b64_json.is_none());
        assert_eq!(resp.images[1].b64_json.as_deref(), Some("aGVsbG8="));
        assert!(resp.images[1].url.is_none());
    }

    #[tokio::test]
    async fn generate_image_http_500_returns_stream_aborted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(
            "openai",
            Some(server.uri()),
            AuthProfile::api_key("sk-test"),
        )
        .unwrap();
        let req = ImageRequest {
            model: "dall-e-3".to_string(),
            prompt: "a cat".to_string(),
            size: None,
            n: None,
        };
        let err = provider.generate_image(req).await.unwrap_err();
        match err {
            ProviderError::StreamAborted(msg) => {
                assert!(msg.contains("HTTP 500"), "unexpected error: {msg}");
                assert!(msg.contains("internal error"));
            }
            other => panic!("expected StreamAborted, got {other}"),
        }
    }

    #[tokio::test]
    async fn synthesize_speech_returns_audio_bytes_with_defaults() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/speech"))
            .and(body_json(serde_json::json!({
                "model": "tts-1",
                "input": "hello world",
                "voice": "alloy",
                "response_format": "mp3",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-mp3-bytes".to_vec()))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(
            "openai",
            Some(server.uri()),
            AuthProfile::api_key("sk-test"),
        )
        .unwrap();
        let req = SpeechRequest {
            model: "tts-1".to_string(),
            input: "hello world".to_string(),
            voice: None,
            format: None,
        };
        let resp = provider.synthesize_speech(req).await.unwrap();
        assert_eq!(resp.audio, b"fake-mp3-bytes");
        assert_eq!(resp.format, "mp3");
    }

    #[tokio::test]
    async fn synthesize_speech_http_500_returns_stream_aborted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/speech"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(
            "openai",
            Some(server.uri()),
            AuthProfile::api_key("sk-test"),
        )
        .unwrap();
        let req = SpeechRequest {
            model: "tts-1".to_string(),
            input: "hello".to_string(),
            voice: Some("nova".to_string()),
            format: Some("opus".to_string()),
        };
        let err = provider.synthesize_speech(req).await.unwrap_err();
        match err {
            ProviderError::StreamAborted(msg) => {
                assert!(msg.contains("HTTP 500"), "unexpected error: {msg}");
                assert!(msg.contains("internal error"));
            }
            other => panic!("expected StreamAborted, got {other}"),
        }
    }

    #[tokio::test]
    async fn chat_maps_context_length_error_to_prompt_too_long() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "code": "context_length_exceeded",
                    "message": "This model's maximum context length is 4096 tokens."
                }
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(
            "openai",
            Some(server.uri()),
            AuthProfile::api_key("sk-test"),
        )
        .unwrap();
        let req = ChatRequest::new("gpt-4", vec![ChatMessage::user("hello")]);
        assert!(matches!(
            provider.chat(req).await,
            Err(ProviderError::PromptTooLong)
        ));
    }
}
