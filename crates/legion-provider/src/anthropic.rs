use crate::auth::AuthProfile;
use crate::provider::Provider;
use crate::types::{
    ChatChunk, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding, FinishReason,
    FunctionCall, ModelInfo, ProviderError, ToolCall,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;

const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// An Anthropic-compatible provider. Works for Anthropic and any provider
/// exposing the `/messages` endpoint (e.g. MiniMax Anthropic endpoint).
pub struct AnthropicProvider {
    id: String,
    base_url: String,
    client: reqwest::Client,
    models: Vec<ModelInfo>,
    extra_headers: HashMap<String, String>,
}

impl AnthropicProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: Option<String>,
        auth: AuthProfile,
    ) -> Result<Self, ProviderError> {
        let key = auth
            .api_key_value()
            .ok_or_else(|| {
                ProviderError::InvalidAuth("Anthropic provider requires an API key".to_string())
            })?
            .to_string();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_str(&key)
                .map_err(|e| ProviderError::InvalidAuth(e.to_string()))?,
        );
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        Ok(Self {
            id: id.into(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
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

    fn messages_url(&self) -> String {
        format!("{}/messages", self.base_url.trim_end_matches('/'))
    }

    fn build_body(&self, req: &ChatRequest) -> serde_json::Value {
        let mut system_parts = Vec::new();
        let mut messages = Vec::with_capacity(req.messages.len());

        let mut i = 0;
        while i < req.messages.len() {
            let m = &req.messages[i];
            if m.role == ChatRole::System {
                let mut part = serde_json::json!({
                    "type": "text",
                    "text": m.content,
                });
                if m.cache_breakpoint {
                    part["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
                system_parts.push(part);
                i += 1;
                continue;
            }

            if m.role == ChatRole::Assistant {
                messages.push(self.build_assistant_message(m));
                i += 1;
                continue;
            }

            // User or tool messages. Group consecutive tool results into one user message.
            let role = "user";
            let mut content_blocks = Vec::new();

            if m.role == ChatRole::User && !m.content.is_empty() {
                content_blocks.push(serde_json::json!({
                    "type": "text",
                    "text": m.content,
                }));
            }
            if m.role == ChatRole::Tool {
                let tool_use_id = m.tool_call_id.clone().unwrap_or_default();
                content_blocks.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": m.content,
                }));
            }
            i += 1;

            // Collapse following tool messages into the same user message.
            while i < req.messages.len() && req.messages[i].role == ChatRole::Tool {
                let tool = &req.messages[i];
                let tool_use_id = tool.tool_call_id.clone().unwrap_or_default();
                content_blocks.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": tool.content,
                }));
                i += 1;
            }

            if !content_blocks.is_empty() {
                messages.push(serde_json::json!({
                    "role": role,
                    "content": serde_json::Value::Array(content_blocks),
                }));
            }
        }

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.max_tokens.unwrap_or(4096),
            "stream": true,
        });

        if !system_parts.is_empty() {
            body["system"] = serde_json::Value::Array(system_parts);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = serde_json::Value::from(t);
        }
        if let Some(p) = req.top_p {
            body["top_p"] = serde_json::Value::from(p);
        }
        if let Some(stop) = &req.stop {
            body["stop_sequences"] = serde_json::to_value(stop).unwrap_or_default();
        }
        if let Some(tools) = &req.tools {
            let tool_specs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tool_specs);
        }
        for (k, v) in &req.extra {
            body[k] = v.clone();
        }

        body
    }

    fn build_assistant_message(&self, m: &crate::types::ChatMessage) -> serde_json::Value {
        match &m.tool_calls {
            Some(calls) if !calls.is_empty() => {
                let mut blocks = Vec::with_capacity(calls.len() + 1);
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": m.content,
                    }));
                }
                for call in calls {
                    let input: serde_json::Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.function.name,
                        "input": input,
                    }));
                }
                serde_json::json!({
                    "role": "assistant",
                    "content": serde_json::Value::Array(blocks),
                })
            }
            _ => serde_json::json!({
                "role": "assistant",
                "content": m.content,
            }),
        }
    }

    fn build_request(&self, url: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
        let mut builder = self.client.post(url).json(&body);
        for (k, v) in &self.extra_headers {
            builder = builder.header(k, v);
        }
        builder
    }
}

fn is_prompt_too_long(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("context_overflow")
        || lower.contains("context length")
        || lower.contains("max tokens")
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn supported_models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let body = self.build_body(&req);
        let response = self
            .build_request(&self.messages_url(), body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            if is_prompt_too_long(&text) {
                return Err(ProviderError::PromptTooLong);
            }
            return Err(ProviderError::StreamAborted(format!(
                "HTTP {status}: {text}"
            )));
        }

        let stream = response
            .bytes_stream()
            .eventsource()
            .scan(AnthropicStreamState::default(), |state, event| {
                let result = match event {
                    Ok(event) => {
                        let parsed: Result<AnthropicEventData, _> =
                            serde_json::from_str(&event.data);
                        match parsed {
                            Ok(data) => data.into_chat_chunk(state),
                            Err(err) => Some(Err(ProviderError::JsonParse(err))),
                        }
                    }
                    Err(err) => Some(Err(ProviderError::SseParse(err.to_string()))),
                };
                // scan terminates the stream on None, so always yield Some(result)
                // and let filter_map drop the inner None values.
                futures::future::ready(Some(result))
            })
            .filter_map(futures::future::ready);

        Ok(Box::pin(stream))
    }

    async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        Err(ProviderError::EmbeddingNotSupported(self.id.clone()))
    }
}

/// Accumulator for Anthropic streaming events.
#[derive(Debug, Default)]
struct AnthropicStreamState {
    current_tool: Option<PartialToolUse>,
    completed_tools: Vec<ToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolUse {
    id: String,
    name: String,
    input_parts: String,
}

#[derive(Debug, Deserialize, Default)]
struct AnthropicEventData {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    content_block: Option<AnthropicContentBlockStart>,
    #[serde(default)]
    delta: Option<AnthropicDelta>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlockStart {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AnthropicDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "type", default)]
    delta_type: Option<String>,
    #[serde(rename = "partial_json", default)]
    partial_json: Option<String>,
    #[serde(rename = "stop_reason", default)]
    stop_reason: Option<String>,
}

impl AnthropicEventData {
    fn into_chat_chunk(
        self,
        state: &mut AnthropicStreamState,
    ) -> Option<Result<ChatChunk, ProviderError>> {
        match self.event_type.as_str() {
            "content_block_start" => {
                if let Some(block) = self.content_block {
                    if block.block_type == "tool_use" {
                        state.current_tool = Some(PartialToolUse {
                            id: block.id.unwrap_or_default(),
                            name: block.name.unwrap_or_default(),
                            input_parts: String::new(),
                        });
                    }
                }
                None
            }
            "content_block_delta" => {
                if let Some(delta) = self.delta {
                    if let Some(text) = delta.text {
                        return Some(Ok(ChatChunk {
                            index: 0,
                            delta: text,
                            finish_reason: None,
                            tool_calls: None,
                        }));
                    }
                    if delta.delta_type.as_deref() == Some("input_json_delta") {
                        if let Some(parts) = delta.partial_json {
                            if let Some(tool) = &mut state.current_tool {
                                tool.input_parts.push_str(&parts);
                            }
                        }
                    }
                }
                None
            }
            "content_block_stop" => {
                if let Some(tool) = state.current_tool.take() {
                    let input: serde_json::Value = serde_json::from_str(&tool.input_parts)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    state.completed_tools.push(ToolCall {
                        id: tool.id,
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: tool.name,
                            arguments: input.to_string(),
                        },
                    });
                }
                None
            }
            "message_delta" => {
                if let Some(delta) = self.delta {
                    let finish_reason = delta.stop_reason.and_then(|r| match r.as_str() {
                        "end_turn" => Some(FinishReason::Stop),
                        "max_tokens" => Some(FinishReason::Length),
                        "tool_use" => Some(FinishReason::ToolCalls),
                        _ => None,
                    });

                    if finish_reason == Some(FinishReason::ToolCalls) {
                        let calls = std::mem::take(&mut state.completed_tools);
                        if calls.is_empty() {
                            return Some(Err(ProviderError::StreamAborted(
                                "tool_use stop reason with no completed tool calls".to_string(),
                            )));
                        }
                        return Some(Ok(ChatChunk {
                            index: 0,
                            delta: String::new(),
                            finish_reason,
                            tool_calls: Some(calls),
                        }));
                    }

                    if finish_reason.is_some() {
                        return Some(Ok(ChatChunk {
                            index: 0,
                            delta: String::new(),
                            finish_reason,
                            tool_calls: None,
                        }));
                    }
                }
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn detects_prompt_too_long_from_error_body() {
        assert!(is_prompt_too_long(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#
        ));
        assert!(is_prompt_too_long(
            r#"{"type":"error","error":{"type":"context_overflow","message":"..."}}"#
        ));
        assert!(!is_prompt_too_long("invalid_api_key"));
    }

    #[test]
    fn build_body_adds_cache_control_to_breakpoint_system_messages() {
        let provider =
            AnthropicProvider::new("anthropic", None, AuthProfile::api_key("sk-test")).unwrap();

        let mut system = ChatMessage::system("You are helpful.");
        system.cache_breakpoint = true;
        let req = ChatRequest::new("claude", vec![system, ChatMessage::user("hi")]);
        let body = provider.build_body(&req);

        let system_parts = body["system"].as_array().unwrap();
        assert_eq!(system_parts[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_body_extracts_system_messages() {
        let provider = AnthropicProvider::new("anthropic", None, AuthProfile::api_key("sk-test"))
            .unwrap()
            .with_model("claude-sonnet-4-6");

        let req = ChatRequest::new(
            "claude-sonnet-4-6",
            vec![
                ChatMessage::system("You are helpful"),
                ChatMessage::user("hello"),
            ],
        );
        let body = provider.build_body(&req);
        assert!(body.get("messages").unwrap().as_array().unwrap().len() == 1);
        assert!(body.get("system").is_some());
    }

    #[test]
    fn build_body_uses_default_max_tokens() {
        let provider =
            AnthropicProvider::new("anthropic", None, AuthProfile::api_key("sk-test")).unwrap();
        let req = ChatRequest::new("claude", vec![ChatMessage::user("hi")]);
        let body = provider.build_body(&req);
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn parses_text_deltas_into_chat_chunks() {
        let data =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#;
        let event: AnthropicEventData = serde_json::from_str(data).unwrap();
        let mut state = AnthropicStreamState::default();
        let chunk = event.into_chat_chunk(&mut state);
        assert!(chunk.is_some());
        assert_eq!(chunk.unwrap().unwrap().delta, "Hi");
    }

    #[test]
    fn build_body_turns_assistant_tool_calls_into_tool_use_blocks() {
        let provider =
            AnthropicProvider::new("anthropic", None, AuthProfile::api_key("sk-test")).unwrap();

        let mut assistant = ChatMessage::assistant("I will run a command.");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "exec".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            },
        }]);

        let tool = ChatMessage {
            role: ChatRole::Tool,
            content: r#"{"exit_code":0,"stdout":"file.txt","stderr":""}"#.into(),
            name: None,
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            cache_breakpoint: false,
        };

        let req = ChatRequest::new(
            "claude",
            vec![ChatMessage::user("list files"), assistant, tool],
        );
        let body = provider.build_body(&req);

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);

        assert_eq!(messages[0]["role"], "user");
        let user_content = messages[0]["content"].as_array().unwrap();
        assert_eq!(user_content[0]["type"], "text");

        let assistant_msg = &messages[1];
        assert_eq!(assistant_msg["role"], "assistant");
        let content = assistant_msg["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "call_1");
        assert_eq!(content[1]["name"], "exec");
        assert_eq!(content[1]["input"]["command"], "ls");

        let tool_msg = &messages[2];
        assert_eq!(tool_msg["role"], "user");
        let tool_content = tool_msg["content"].as_array().unwrap();
        assert_eq!(tool_content.len(), 1);
        assert_eq!(tool_content[0]["type"], "tool_result");
        assert_eq!(tool_content[0]["tool_use_id"], "call_1");
    }

    #[test]
    fn accumulates_streaming_tool_use_blocks() {
        let mut state = AnthropicStreamState::default();

        let start = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_2","name":"exec","input":{}}}"#;
        let data: AnthropicEventData = serde_json::from_str(start).unwrap();
        assert!(data.into_chat_chunk(&mut state).is_none());

        let delta = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#;
        let data: AnthropicEventData = serde_json::from_str(delta).unwrap();
        assert!(data.into_chat_chunk(&mut state).is_none());

        let stop = r#"{"type":"content_block_stop","index":1}"#;
        let data: AnthropicEventData = serde_json::from_str(stop).unwrap();
        assert!(data.into_chat_chunk(&mut state).is_none());

        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#;
        let data: AnthropicEventData = serde_json::from_str(message_delta).unwrap();
        let chunk = data.into_chat_chunk(&mut state).unwrap().unwrap();
        assert_eq!(chunk.finish_reason, Some(FinishReason::ToolCalls));
        let calls = chunk.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_2");
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[tokio::test]
    async fn chat_maps_prompt_too_long_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "prompt is too long: 200000 tokens > 100000 maximum"
                }
            })))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(
            "anthropic",
            Some(server.uri()),
            AuthProfile::api_key("sk-test"),
        )
        .unwrap();
        let req = ChatRequest::new("claude", vec![ChatMessage::user("hello")]);
        assert!(matches!(
            provider.chat(req).await,
            Err(ProviderError::PromptTooLong)
        ));
    }
}
