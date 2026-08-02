use crate::auth::AuthProfile;
use crate::eventstream::EventStreamDecoder;
use crate::http;
use crate::provider::Provider;
use crate::sigv4::{AwsCreds, sign_request};
use crate::types::{
    ChatChunk, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding, FinishReason, ModelInfo,
    PartialToolCall, ProviderError, ToolCallAccumulator, merged_system_text, parse_tool_arguments,
    tool_run_end,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use std::time::SystemTime;

/// AWS service name used for SigV4 signing.
const BEDROCK_SERVICE: &str = "bedrock";

/// Titan embedding model invoked when the request does not name one.
const DEFAULT_EMBED_MODEL: &str = "amazon.titan-embed-text-v1";

/// Static chat model catalog advertised when no explicit default model is set.
const BEDROCK_CHAT_MODELS: [&str; 2] =
    ["anthropic.claude-sonnet-4-5", "anthropic.claude-haiku-4-5"];

/// Native provider for the AWS Bedrock Runtime API.
///
/// Uses the ConverseStream endpoint (`POST /model/{model}/converse-stream`)
/// for chat, the Titan invoke endpoint (`POST /model/{model}/invoke`) for
/// embeddings, and authenticates every request with AWS SigV4. Responses are
/// AWS event-stream binary frames decoded by `EventStreamDecoder`.
pub struct BedrockProvider {
    id: String,
    base_url: String,
    client: reqwest::Client,
    creds: AwsCreds,
    default_model: Option<String>,
}

impl BedrockProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: Option<String>,
        auth: AuthProfile,
    ) -> Result<Self, ProviderError> {
        let creds = auth.aws_sigv4_value().ok_or_else(|| {
            ProviderError::InvalidAuth(
                "Bedrock provider requires an aws_sigv4 auth profile".to_string(),
            )
        })?;
        let base_url = base_url
            .unwrap_or_else(|| format!("https://bedrock-runtime.{}.amazonaws.com", creds.region));

        Ok(Self {
            id: id.into(),
            base_url,
            client: reqwest::Client::new(),
            creds,
            default_model: None,
        })
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    fn converse_url(&self, model: &str) -> String {
        format!(
            "{}/model/{model}/converse-stream",
            self.base_url.trim_end_matches('/')
        )
    }

    fn invoke_url(&self, model: &str) -> String {
        format!(
            "{}/model/{model}/invoke",
            self.base_url.trim_end_matches('/')
        )
    }

    /// Build a SigV4-signed POST request. The `content-type` / `accept`
    /// headers participate in the signature, so they are passed here rather
    /// than set as client defaults.
    fn signed_post(
        &self,
        url: &str,
        body: &[u8],
        accept: &str,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), accept.to_string()),
        ];
        let signed = sign_request(
            "POST",
            url,
            &headers,
            body,
            &self.creds,
            BEDROCK_SERVICE,
            SystemTime::now(),
        )?;
        let mut builder = self
            .client
            .post(url)
            .body(body.to_vec())
            .header("content-type", "application/json")
            .header("accept", accept);
        for (key, value) in signed {
            builder = builder.header(key, value);
        }
        Ok(builder)
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn supported_models(&self) -> Vec<ModelInfo> {
        let mut models: Vec<ModelInfo> = BEDROCK_CHAT_MODELS
            .iter()
            .map(|id| {
                let mut info = ModelInfo::new(*id, "bedrock");
                info.context_window = Some(200_000);
                info.supports_tool_use = Some(true);
                info
            })
            .collect();
        models.push(ModelInfo::new(DEFAULT_EMBED_MODEL, "bedrock"));
        if let Some(default) = &self.default_model
            && !models.iter().any(|m| m.id == *default)
        {
            models.push(ModelInfo::new(default, &self.id));
        }
        models
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let body = serde_json::to_vec(&to_converse_request(&req))?;
        let url = self.converse_url(&req.model);
        tracing::debug!(url = %url, body = %String::from_utf8_lossy(&body), "Bedrock chat request");

        let response = self
            .signed_post(&url, &body, "application/vnd.aws.eventstream")?
            .send()
            .await?;
        let response = http::check_status(response, false).await?;

        // The response is an AWS event-stream: buffer bytes, decode frames,
        // and map each frame's JSON payload to a ChatChunk. Frames that carry
        // no chunk (tool-use accumulation, metadata) are skipped inside the
        // unfold loop.
        let byte_stream = Box::pin(response.bytes_stream());
        let stream = futures::stream::unfold(
            (
                byte_stream,
                EventStreamDecoder::new(),
                BedrockStreamState::default(),
                false,
            ),
            |(mut bytes, mut decoder, mut state, mut done)| async move {
                loop {
                    if done {
                        return None;
                    }
                    match decoder.next_frame() {
                        Ok(Some((event_type, payload))) => {
                            let json: serde_json::Value = match serde_json::from_slice(&payload) {
                                Ok(value) => value,
                                Err(err) => {
                                    done = true;
                                    return Some((
                                        Err(ProviderError::JsonParse(err)),
                                        (bytes, decoder, state, done),
                                    ));
                                }
                            };
                            match converse_event_to_chunk(&event_type, &json, &mut state) {
                                Some(Ok(chunk)) => {
                                    if chunk.finish_reason.is_some() {
                                        done = true;
                                    }
                                    return Some((Ok(chunk), (bytes, decoder, state, done)));
                                }
                                Some(Err(err)) => {
                                    done = true;
                                    return Some((Err(err), (bytes, decoder, state, done)));
                                }
                                None => continue,
                            }
                        }
                        Ok(None) => match bytes.next().await {
                            Some(Ok(chunk)) => decoder.push(&chunk),
                            Some(Err(err)) => {
                                done = true;
                                return Some((
                                    Err(ProviderError::Http(err)),
                                    (bytes, decoder, state, done),
                                ));
                            }
                            None => return None,
                        },
                        Err(err) => {
                            done = true;
                            return Some((Err(err), (bytes, decoder, state, done)));
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        // Titan embeds one input per call, so loop over the inputs.
        let model = if req.model.is_empty() {
            DEFAULT_EMBED_MODEL
        } else {
            &req.model
        };
        let url = self.invoke_url(model);
        let mut embeddings = Vec::with_capacity(req.input.len());
        for (index, input_text) in req.input.iter().enumerate() {
            let body = serde_json::to_vec(&serde_json::json!({ "inputText": input_text }))?;
            let response = self
                .signed_post(&url, &body, "application/json")?
                .send()
                .await?;
            let response = http::check_status(response, false).await?;
            let payload: TitanEmbedResponse = response.json().await?;
            embeddings.push(Embedding {
                index,
                embedding: payload.embedding,
            });
        }
        Ok(embeddings)
    }
}

/// Convert a provider-agnostic chat request into the Bedrock Converse wire
/// format.
///
/// System messages are concatenated into a single `system` block; assistant
/// tool calls become `toolUse` content blocks; tool results become
/// `toolResult` blocks grouped into one `user` message per consecutive run.
/// This is a pure function so the mapping can be unit-tested without any I/O.
fn to_converse_request(req: &ChatRequest) -> serde_json::Value {
    let system_text = merged_system_text(&req.messages);

    let mut messages: Vec<serde_json::Value> = Vec::new();
    let mut i = 0;
    while i < req.messages.len() {
        let m = &req.messages[i];
        match m.role {
            ChatRole::System => {
                i += 1;
            }
            ChatRole::User => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{ "text": m.content }],
                }));
                i += 1;
            }
            ChatRole::Assistant => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({ "text": m.content }));
                }
                if let Some(calls) = &m.tool_calls {
                    for call in calls {
                        let input = parse_tool_arguments(&call.function.arguments);
                        blocks.push(serde_json::json!({
                            "toolUse": {
                                "toolUseId": call.id,
                                "name": call.function.name,
                                "input": input,
                            }
                        }));
                    }
                }
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": blocks,
                }));
                i += 1;
            }
            ChatRole::Tool => {
                // Consecutive tool results must be merged into one user message.
                let end = tool_run_end(&req.messages, i);
                let mut blocks = Vec::new();
                while i < end {
                    let tool = &req.messages[i];
                    blocks.push(serde_json::json!({
                        "toolResult": {
                            "toolUseId": tool.tool_call_id.clone().unwrap_or_default(),
                            "content": [{ "text": tool.content }],
                        }
                    }));
                    i += 1;
                }
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": blocks,
                }));
            }
        }
    }

    let mut body = serde_json::json!({ "messages": messages });
    if !system_text.is_empty() {
        body["system"] = serde_json::json!([{ "text": system_text }]);
    }

    let mut inference = serde_json::Map::new();
    if let Some(temperature) = req.temperature {
        inference.insert(
            "temperature".to_string(),
            serde_json::Value::from(temperature),
        );
    }
    if let Some(max_tokens) = req.max_tokens {
        inference.insert("maxTokens".to_string(), serde_json::Value::from(max_tokens));
    }
    if let Some(top_p) = req.top_p {
        inference.insert("topP".to_string(), serde_json::Value::from(top_p));
    }
    if let Some(stop) = &req.stop {
        inference.insert(
            "stopSequences".to_string(),
            serde_json::to_value(stop).unwrap_or_default(),
        );
    }
    if !inference.is_empty() {
        body["inferenceConfig"] = serde_json::Value::Object(inference);
    }

    if let Some(tools) = &req.tools {
        let specs: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": { "json": t.parameters },
                    }
                })
            })
            .collect();
        body["toolConfig"] = serde_json::json!({ "tools": specs });
    }
    req.apply_extra_to(&mut body);

    body
}

/// Accumulator for ConverseStream tool-use blocks, keyed by content block
/// index (Bedrock can stream multiple tool calls in parallel blocks).
#[derive(Debug, Default)]
struct BedrockStreamState {
    tool_uses: ToolCallAccumulator,
}

fn content_block_index(payload: &serde_json::Value) -> usize {
    payload["contentBlockIndex"].as_u64().unwrap_or(0) as usize
}

/// Map one decoded ConverseStream event to a `ChatChunk`.
///
/// Returns `None` for events that only update state (tool-use start/delta,
/// metadata), `Some(Err(..))` for exception frames, and `Some(Ok(chunk))`
/// for text deltas, completed tool calls, and stop reasons. Pure function so
/// every branch can be unit-tested without any I/O.
fn converse_event_to_chunk(
    event_type: &str,
    payload: &serde_json::Value,
    state: &mut BedrockStreamState,
) -> Option<Result<ChatChunk, ProviderError>> {
    match event_type {
        "contentBlockStart" => {
            let index = content_block_index(payload);
            if let Some(tool_use) = payload.get("start").and_then(|s| s.get("toolUse")) {
                state.tool_uses.start(
                    index,
                    PartialToolCall {
                        id: tool_use["toolUseId"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        kind: "function".to_string(),
                        name: tool_use["name"].as_str().unwrap_or_default().to_string(),
                        arguments: String::new(),
                    },
                );
            }
            None
        }
        "contentBlockDelta" => {
            let index = content_block_index(payload);
            if let Some(text) = payload["delta"]["text"].as_str() {
                return Some(Ok(ChatChunk {
                    index,
                    delta: text.to_string(),
                    finish_reason: None,
                    tool_calls: None,
                }));
            }
            // Tool-use input arrives as JSON string fragments; accumulate.
            if let Some(input) = payload["delta"]["toolUse"]["input"].as_str() {
                state.tool_uses.append_arguments(index, input);
            }
            None
        }
        "contentBlockStop" => {
            let index = content_block_index(payload);
            let tool = state.tool_uses.finish(index)?;
            Some(Ok(ChatChunk {
                index,
                delta: String::new(),
                finish_reason: None,
                tool_calls: Some(vec![tool]),
            }))
        }
        "messageStop" => {
            let finish_reason = match payload["stopReason"].as_str().unwrap_or_default() {
                "end_turn" => Some(FinishReason::Stop),
                "max_tokens" => Some(FinishReason::Length),
                "tool_use" => Some(FinishReason::ToolCalls),
                "content_filtered" | "guardrail_intervened" => Some(FinishReason::ContentFilter),
                _ => None,
            };
            finish_reason.map(|reason| {
                Ok(ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(reason),
                    tool_calls: None,
                })
            })
        }
        other if other.ends_with("Exception") || other.ends_with("Error") => {
            let message = payload["message"].as_str().unwrap_or(other);
            Some(Err(ProviderError::StreamAborted(format!(
                "bedrock stream error ({other}): {message}"
            ))))
        }
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct TitanEmbedResponse {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventstream::encode_frame;
    use crate::types::{ChatMessage, FunctionCall, ToolCall, ToolDefinition};
    use std::collections::HashMap;
    use wiremock::matchers::{header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_auth() -> AuthProfile {
        AuthProfile::aws_sigv4("AKIDEXAMPLE", "secret", None, "us-east-1")
    }

    fn assistant_with_tool_call() -> ChatMessage {
        let mut m = ChatMessage::assistant("let me check");
        m.tool_calls = Some(vec![ToolCall {
            id: "tooluse_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "exec".to_string(),
                arguments: r#"{"cmd":"ls"}"#.to_string(),
            },
        }]);
        m
    }

    fn tool_result(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Tool,
            content: content.to_string(),
            name: None,
            tool_calls: None,
            tool_call_id: Some(id.to_string()),
            cache_breakpoint: false,
        }
    }

    #[test]
    fn to_converse_request_merges_system_messages() {
        let req = ChatRequest::new(
            "anthropic.claude-sonnet-4-5",
            vec![
                ChatMessage::system("be helpful"),
                ChatMessage::system("be concise"),
                ChatMessage::user("hi"),
            ],
        );
        let body = to_converse_request(&req);
        assert_eq!(body["system"][0]["text"], "be helpful\n\nbe concise");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn to_converse_request_maps_roles_and_tool_calls() {
        let req = ChatRequest::new(
            "anthropic.claude-sonnet-4-5",
            vec![
                ChatMessage::user("ls?"),
                assistant_with_tool_call(),
                tool_result("tooluse_1", "file1 file2"),
                tool_result("tooluse_2", "second result"),
            ],
        );
        let body = to_converse_request(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);

        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        let blocks = assistant["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "let me check");
        assert_eq!(blocks[1]["toolUse"]["toolUseId"], "tooluse_1");
        assert_eq!(blocks[1]["toolUse"]["name"], "exec");
        assert_eq!(blocks[1]["toolUse"]["input"]["cmd"], "ls");

        // Consecutive tool results collapse into a single user message.
        let tool_msg = &messages[2];
        assert_eq!(tool_msg["role"], "user");
        let results = tool_msg["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["toolResult"]["toolUseId"], "tooluse_1");
        assert_eq!(
            results[0]["toolResult"]["content"][0]["text"],
            "file1 file2"
        );
        assert_eq!(results[1]["toolResult"]["toolUseId"], "tooluse_2");
    }

    #[test]
    fn to_converse_request_invalid_tool_arguments_become_empty_object() {
        let mut m = ChatMessage::assistant("");
        m.tool_calls = Some(vec![ToolCall {
            id: "c".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "exec".to_string(),
                arguments: "not json".to_string(),
            },
        }]);
        let req = ChatRequest::new("anthropic.claude-sonnet-4-5", vec![m]);
        let body = to_converse_request(&req);
        assert_eq!(
            body["messages"][0]["content"][0]["toolUse"]["input"],
            serde_json::json!({})
        );
    }

    #[test]
    fn to_converse_request_maps_inference_config_and_tools() {
        let mut req =
            ChatRequest::new("anthropic.claude-sonnet-4-5", vec![ChatMessage::user("hi")]);
        req.temperature = Some(0.7);
        req.max_tokens = Some(256);
        req.top_p = Some(0.9);
        req.stop = Some(vec!["END".to_string()]);
        req.tools = Some(vec![ToolDefinition {
            name: "exec".to_string(),
            description: "run a command".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }]);

        let body = to_converse_request(&req);
        let temperature = body["inferenceConfig"]["temperature"].as_f64().unwrap();
        assert!(
            (temperature - 0.7).abs() < 1e-6,
            "temperature = {temperature}"
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 256);
        let top_p = body["inferenceConfig"]["topP"].as_f64().unwrap();
        assert!((top_p - 0.9).abs() < 1e-6, "top_p = {top_p}");
        assert_eq!(body["inferenceConfig"]["stopSequences"][0], "END");

        let spec = &body["toolConfig"]["tools"][0]["toolSpec"];
        assert_eq!(spec["name"], "exec");
        assert_eq!(spec["description"], "run a command");
        assert_eq!(spec["inputSchema"]["json"]["type"], "object");
    }

    #[test]
    fn to_converse_request_omits_optional_sections_when_unset() {
        let req = ChatRequest::new("anthropic.claude-sonnet-4-5", vec![ChatMessage::user("hi")]);
        let body = to_converse_request(&req);
        assert!(body.get("system").is_none());
        assert!(body.get("inferenceConfig").is_none());
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn converse_event_text_delta_produces_chunk() {
        let mut state = BedrockStreamState::default();
        let payload = serde_json::json!({"contentBlockIndex": 0, "delta": {"text": "Hello"}});
        let chunk = converse_event_to_chunk("contentBlockDelta", &payload, &mut state)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.index, 0);
        assert_eq!(chunk.delta, "Hello");
        assert!(chunk.finish_reason.is_none());
    }

    #[test]
    fn converse_event_accumulates_tool_use_across_frames() {
        let mut state = BedrockStreamState::default();

        let start = serde_json::json!({
            "contentBlockIndex": 1,
            "start": {"toolUse": {"toolUseId": "tooluse_1", "name": "exec"}}
        });
        assert!(converse_event_to_chunk("contentBlockStart", &start, &mut state).is_none());

        let delta1 = serde_json::json!({
            "contentBlockIndex": 1,
            "delta": {"toolUse": {"input": "{\"cmd\":"}}
        });
        assert!(converse_event_to_chunk("contentBlockDelta", &delta1, &mut state).is_none());
        let delta2 = serde_json::json!({
            "contentBlockIndex": 1,
            "delta": {"toolUse": {"input": "\"ls\"}"}}
        });
        assert!(converse_event_to_chunk("contentBlockDelta", &delta2, &mut state).is_none());

        let stop = serde_json::json!({"contentBlockIndex": 1});
        let chunk = converse_event_to_chunk("contentBlockStop", &stop, &mut state)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.index, 1);
        let calls = chunk.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tooluse_1");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"cmd":"ls"}"#);
    }

    #[test]
    fn converse_event_text_block_stop_produces_no_chunk() {
        let mut state = BedrockStreamState::default();
        let stop = serde_json::json!({"contentBlockIndex": 0});
        assert!(converse_event_to_chunk("contentBlockStop", &stop, &mut state).is_none());
    }

    #[test]
    fn converse_event_maps_stop_reasons() {
        let cases = [
            ("end_turn", FinishReason::Stop),
            ("max_tokens", FinishReason::Length),
            ("tool_use", FinishReason::ToolCalls),
            ("content_filtered", FinishReason::ContentFilter),
            ("guardrail_intervened", FinishReason::ContentFilter),
        ];
        for (stop_reason, expected) in cases {
            let mut state = BedrockStreamState::default();
            let payload = serde_json::json!({"stopReason": stop_reason});
            let chunk = converse_event_to_chunk("messageStop", &payload, &mut state)
                .unwrap()
                .unwrap();
            assert_eq!(
                chunk.finish_reason,
                Some(expected),
                "stopReason = {stop_reason}"
            );
        }
    }

    #[test]
    fn converse_event_unknown_stop_reason_and_metadata_are_ignored() {
        let mut state = BedrockStreamState::default();
        let payload = serde_json::json!({"stopReason": "stop_sequence"});
        assert!(converse_event_to_chunk("messageStop", &payload, &mut state).is_none());

        let metadata = serde_json::json!({"usage": {"inputTokens": 1}});
        assert!(converse_event_to_chunk("metadata", &metadata, &mut state).is_none());
    }

    #[test]
    fn converse_event_exception_frame_produces_error() {
        let mut state = BedrockStreamState::default();
        let payload = serde_json::json!({"message": "model exploded"});
        match converse_event_to_chunk("modelStreamErrorException", &payload, &mut state) {
            Some(Err(ProviderError::StreamAborted(msg))) => {
                assert!(msg.contains("modelStreamErrorException"), "msg = {msg}");
                assert!(msg.contains("model exploded"), "msg = {msg}");
            }
            other => panic!("expected StreamAborted, got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_non_sigv4_auth() {
        match BedrockProvider::new("bedrock", None, AuthProfile::api_key("k")) {
            Err(ProviderError::InvalidAuth(_)) => {}
            Err(err) => panic!("expected InvalidAuth, got {err}"),
            Ok(_) => panic!("api_key auth must be rejected"),
        }
    }

    #[test]
    fn default_base_url_uses_credential_region() {
        let provider = BedrockProvider::new(
            "bedrock",
            None,
            AuthProfile::aws_sigv4("AK", "SK", None, "eu-west-1"),
        )
        .unwrap();
        assert_eq!(
            provider.base_url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        );
    }

    #[test]
    fn supported_models_includes_catalog_and_dedup_default() {
        let provider = BedrockProvider::new("bedrock", None, test_auth())
            .unwrap()
            .with_model("anthropic.claude-sonnet-4-5");
        let models = provider.supported_models();
        // Default is already in the catalog: 2 chat + 1 embed.
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "anthropic.claude-sonnet-4-5");
        assert_eq!(models[0].provider, "bedrock");
        assert_eq!(models[0].context_window, Some(200_000));
        assert_eq!(models[0].supports_tool_use, Some(true));
        assert_eq!(models[2].id, "amazon.titan-embed-text-v1");

        let provider = BedrockProvider::new("bedrock", None, test_auth())
            .unwrap()
            .with_model("custom.model");
        let models = provider.supported_models();
        assert_eq!(models.len(), 4);
        assert_eq!(models[3].id, "custom.model");
    }

    /// Event headers shared by every ConverseStream frame in the mock stream.
    const EVENT_HEADERS: [(&str, &str); 3] = [
        (":event-type", ""),
        (":message-type", "event"),
        (":content-type", "application/json"),
    ];

    fn event_frame(event_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = EVENT_HEADERS;
        headers[0].1 = event_type;
        encode_frame(&headers, payload.as_bytes())
    }

    #[tokio::test]
    async fn chat_decodes_event_stream_with_tool_use() {
        let server = MockServer::start().await;
        let mut body = Vec::new();
        body.extend(event_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":" world"}}"#,
        ));
        body.extend(event_frame(
            "contentBlockStart",
            r#"{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"tooluse_1","name":"exec"}}}"#,
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"cmd\":"}}}"#,
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"\"ls\"}"}}}"#,
        ));
        body.extend(event_frame(
            "contentBlockStop",
            r#"{"contentBlockIndex":1}"#,
        ));
        body.extend(event_frame("messageStop", r#"{"stopReason":"tool_use"}"#));

        Mock::given(method("POST"))
            .and(path("/model/anthropic.claude-sonnet-4-5/converse-stream"))
            .and(header_regex("authorization", "AWS4-HMAC-SHA256"))
            .and(header_regex("x-amz-date", r"\d{8}T\d{6}Z"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.aws.eventstream")
                    .set_body_bytes(body),
            )
            .mount(&server)
            .await;

        let provider = BedrockProvider::new("bedrock", Some(server.uri()), test_auth()).unwrap();
        let req = ChatRequest::new("anthropic.claude-sonnet-4-5", vec![ChatMessage::user("hi")]);
        let stream = provider.chat(req).await.unwrap();
        let chunks: Vec<ChatChunk> = stream
            .collect::<Vec<Result<ChatChunk, ProviderError>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].delta, "Hello");
        assert_eq!(chunks[1].delta, " world");

        let tool_chunk = &chunks[2];
        assert_eq!(tool_chunk.index, 1);
        let calls = tool_chunk.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].id, "tooluse_1");
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"cmd":"ls"}"#);

        assert_eq!(chunks[3].finish_reason, Some(FinishReason::ToolCalls));
    }

    #[tokio::test]
    async fn chat_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let provider = BedrockProvider::new("bedrock", Some(server.uri()), test_auth()).unwrap();
        let req = ChatRequest::new("anthropic.claude-sonnet-4-5", vec![ChatMessage::user("hi")]);
        match provider.chat(req).await {
            Err(ProviderError::StreamAborted(_)) => {}
            Err(err) => panic!("expected StreamAborted, got {err}"),
            Ok(_) => panic!("403 must fail"),
        }
    }

    #[tokio::test]
    async fn embed_parses_titan_responses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model/amazon.titan-embed-text-v1/invoke"))
            .and(header_regex("authorization", "AWS4-HMAC-SHA256"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "embedding": [0.1, 0.2] })),
            )
            .mount(&server)
            .await;

        let provider = BedrockProvider::new("bedrock", Some(server.uri()), test_auth()).unwrap();
        let req = EmbedRequest {
            model: "amazon.titan-embed-text-v1".to_string(),
            input: vec!["a".to_string(), "b".to_string()],
            extra: HashMap::new(),
        };
        let embeddings = provider.embed(req).await.unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].index, 0);
        assert_eq!(embeddings[0].embedding, vec![0.1, 0.2]);
        assert_eq!(embeddings[1].index, 1);
    }
}
