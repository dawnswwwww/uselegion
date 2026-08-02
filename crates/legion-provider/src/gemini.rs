use crate::auth::AuthProfile;
use crate::http;
use crate::provider::Provider;
use crate::types::{
    ChatChunk, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding, FinishReason,
    FunctionCall, ModelInfo, ProviderError, ToolCall, merged_system_text, parse_tool_arguments,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Static model catalog advertised when no explicit default model is set.
const GEMINI_MODELS: [&str; 3] = ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"];

/// Native provider for the Google Generative Language API (v1beta).
///
/// Uses `:streamGenerateContent?alt=sse` for chat, `:batchEmbedContents` for
/// embeddings, and authenticates via the `x-goog-api-key` header.
pub struct GeminiProvider {
    id: String,
    base_url: String,
    client: reqwest::Client,
    default_model: Option<String>,
}

impl GeminiProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: Option<String>,
        auth: AuthProfile,
    ) -> Result<Self, ProviderError> {
        let key = auth
            .api_key_value()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                ProviderError::InvalidAuth("Gemini provider requires a non-empty API key".into())
            })?;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-goog-api-key"),
            http::header_value(key)?,
        );

        let client = http::json_client(headers)?;

        Ok(Self {
            id: id.into(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_GEMINI_BASE_URL.to_string()),
            client,
            default_model: None,
        })
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    fn chat_url(&self, model: &str) -> String {
        format!(
            "{}/v1beta/models/{model}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/')
        )
    }

    fn embed_url(&self, model: &str) -> String {
        format!(
            "{}/v1beta/models/{model}:batchEmbedContents",
            self.base_url.trim_end_matches('/')
        )
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn supported_models(&self) -> Vec<ModelInfo> {
        let mut models: Vec<ModelInfo> = GEMINI_MODELS
            .iter()
            .map(|id| {
                let mut info = ModelInfo::new(*id, "gemini");
                info.context_window = Some(1_000_000);
                info.supports_tool_use = Some(true);
                info
            })
            .collect();
        if let Some(default) = &self.default_model
            && !models.iter().any(|m| m.id == *default)
        {
            models.push(ModelInfo::new(default, &self.id));
        }
        models
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let mut body = serde_json::to_value(to_gemini_request(&req))?;
        req.apply_extra_to(&mut body);
        let url = self.chat_url(&req.model);
        tracing::debug!(url = %url, body = %serde_json::to_string(&body).unwrap_or_default(), "Gemini chat request");

        let response =
            http::check_status(self.client.post(&url).json(&body).send().await?, false).await?;

        Ok(http::sse_stream(response, (), |_state, data| {
            Some(
                http::parse_sse_json::<GeminiStreamChunk>(data)
                    .map(|chunk| chunk.into_chat_chunk()),
            )
        }))
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        let requests: Vec<serde_json::Value> = req
            .input
            .iter()
            .map(|text| {
                serde_json::json!({
                    "model": format!("models/{}", req.model),
                    "content": { "parts": [{ "text": text }] }
                })
            })
            .collect();
        let body = serde_json::json!({ "requests": requests });

        let response = self
            .client
            .post(self.embed_url(&req.model))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let payload: GeminiEmbedResponse = response.json().await?;
        Ok(payload
            .embeddings
            .into_iter()
            .enumerate()
            .map(|(index, e)| Embedding {
                index,
                embedding: e.values,
            })
            .collect())
    }
}

/// Convert a provider-agnostic chat request into the Gemini wire format.
///
/// System messages are concatenated into `systemInstruction`; assistant tool
/// calls become `functionCall` parts; tool results become `functionResponse`
/// parts on a synthetic `user` turn. This is a pure function so the mapping
/// can be unit-tested without any I/O.
fn to_gemini_request(req: &ChatRequest) -> GeminiRequest {
    // Map tool-call ids to function names so tool results can reference the
    // function they answer (Gemini keys `functionResponse` by name).
    let mut call_names: HashMap<&str, &str> = HashMap::new();
    for m in &req.messages {
        if let Some(calls) = &m.tool_calls {
            for call in calls {
                call_names.insert(&call.id, &call.function.name);
            }
        }
    }

    let system_text = merged_system_text(&req.messages);
    let system_instruction = if system_text.is_empty() {
        None
    } else {
        Some(GeminiContent {
            role: None,
            parts: vec![GeminiPart {
                text: Some(system_text),
                function_call: None,
                function_response: None,
            }],
        })
    };

    let mut contents = Vec::new();
    for m in &req.messages {
        match m.role {
            ChatRole::System => {}
            ChatRole::User => contents.push(GeminiContent {
                role: Some("user".to_string()),
                parts: vec![GeminiPart {
                    text: Some(m.content.clone()),
                    function_call: None,
                    function_response: None,
                }],
            }),
            ChatRole::Assistant => {
                let mut parts = Vec::new();
                if !m.content.is_empty() {
                    parts.push(GeminiPart {
                        text: Some(m.content.clone()),
                        function_call: None,
                        function_response: None,
                    });
                }
                if let Some(calls) = &m.tool_calls {
                    for call in calls {
                        let args = parse_tool_arguments(&call.function.arguments);
                        parts.push(GeminiPart {
                            text: None,
                            function_call: Some(GeminiFunctionCall {
                                name: call.function.name.clone(),
                                args,
                            }),
                            function_response: None,
                        });
                    }
                }
                contents.push(GeminiContent {
                    role: Some("model".to_string()),
                    parts,
                });
            }
            ChatRole::Tool => {
                let name = m
                    .tool_call_id
                    .as_deref()
                    .map(|id| {
                        call_names
                            .get(id)
                            .map(|n| (*n).to_string())
                            .unwrap_or_else(|| id.to_string())
                    })
                    .or_else(|| m.name.clone())
                    .unwrap_or_else(|| "tool".to_string());
                contents.push(GeminiContent {
                    role: Some("user".to_string()),
                    parts: vec![GeminiPart {
                        text: None,
                        function_call: None,
                        function_response: Some(GeminiFunctionResponse {
                            name,
                            response: serde_json::json!({ "content": m.content }),
                        }),
                    }],
                });
            }
        }
    }

    let tools = req.tools.as_ref().map(|tools| {
        vec![GeminiTool {
            function_declarations: tools
                .iter()
                .map(|t| GeminiFunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                })
                .collect(),
        }]
    });

    let generation_config = if req.temperature.is_some()
        || req.max_tokens.is_some()
        || req.top_p.is_some()
        || req.stop.is_some()
    {
        Some(GeminiGenerationConfig {
            temperature: req.temperature,
            max_output_tokens: req.max_tokens,
            top_p: req.top_p,
            stop_sequences: req.stop.clone(),
        })
    } else {
        None
    };

    GeminiRequest {
        system_instruction,
        contents,
        tools,
        generation_config,
    }
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    content: Option<GeminiResponseContent>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiResponseContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GeminiResponsePart {
    text: Option<String>,
    function_call: Option<GeminiResponseFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponseFunctionCall {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

impl GeminiStreamChunk {
    fn into_chat_chunk(self) -> ChatChunk {
        let candidate = self.candidates.into_iter().next().unwrap_or_default();
        let finish_reason = candidate.finish_reason.and_then(|r| match r.as_str() {
            "STOP" => Some(FinishReason::Stop),
            "MAX_TOKENS" => Some(FinishReason::Length),
            "SAFETY" => Some(FinishReason::ContentFilter),
            _ => None,
        });

        let mut delta = String::new();
        let mut calls = Vec::new();
        if let Some(content) = candidate.content {
            for (part_index, part) in content.parts.into_iter().enumerate() {
                if let Some(text) = part.text {
                    delta.push_str(&text);
                }
                if let Some(call) = part.function_call {
                    calls.push(ToolCall {
                        id: format!("call_{}_{part_index}", call.name),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: call.name,
                            arguments: call.args.to_string(),
                        },
                    });
                }
            }
        }

        ChatChunk {
            index: candidate.index,
            delta,
            finish_reason,
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
        }
    }
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedResponse {
    embeddings: Vec<GeminiEmbedding>,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedding {
    values: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ToolDefinition};
    use futures::StreamExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn assistant_with_tool_call() -> ChatMessage {
        let mut m = ChatMessage::assistant("let me check");
        m.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "exec".to_string(),
                arguments: r#"{"cmd":"ls"}"#.to_string(),
            },
        }]);
        m
    }

    fn tool_result() -> ChatMessage {
        ChatMessage {
            role: ChatRole::Tool,
            content: "file1 file2".to_string(),
            name: None,
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
            cache_breakpoint: false,
        }
    }

    #[test]
    fn to_gemini_request_merges_system_messages() {
        let req = ChatRequest::new(
            "gemini-2.5-flash",
            vec![
                ChatMessage::system("be helpful"),
                ChatMessage::system("be concise"),
                ChatMessage::user("hi"),
            ],
        );
        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "be helpful\n\nbe concise"
        );
        // System messages must not leak into contents.
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn to_gemini_request_maps_roles() {
        let req = ChatRequest::new(
            "gemini-2.5-flash",
            vec![
                ChatMessage::user("question"),
                ChatMessage::assistant("answer"),
            ],
        );
        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "question");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "answer");
    }

    #[test]
    fn to_gemini_request_maps_tool_calls_to_function_call_parts() {
        let req = ChatRequest::new(
            "gemini-2.5-flash",
            vec![ChatMessage::user("ls?"), assistant_with_tool_call()],
        );
        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "let me check");
        assert_eq!(parts[1]["functionCall"]["name"], "exec");
        assert_eq!(parts[1]["functionCall"]["args"]["cmd"], "ls");
    }

    #[test]
    fn to_gemini_request_maps_tool_results_to_function_response() {
        let req = ChatRequest::new(
            "gemini-2.5-flash",
            vec![
                ChatMessage::user("ls?"),
                assistant_with_tool_call(),
                tool_result(),
            ],
        );
        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        let tool_content = &body["contents"][2];
        assert_eq!(tool_content["role"], "user");
        let response = &tool_content["parts"][0]["functionResponse"];
        assert_eq!(response["name"], "exec");
        assert_eq!(response["response"]["content"], "file1 file2");
    }

    #[test]
    fn to_gemini_request_falls_back_to_tool_call_id_for_unknown_call() {
        let mut orphan = tool_result();
        orphan.tool_call_id = Some("call_unknown".to_string());
        let req = ChatRequest::new("gemini-2.5-flash", vec![orphan]);
        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][0]["functionResponse"]["name"],
            "call_unknown"
        );
    }

    #[test]
    fn to_gemini_request_invalid_tool_arguments_become_empty_object() {
        let mut m = ChatMessage::assistant("");
        m.tool_calls = Some(vec![ToolCall {
            id: "c".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "exec".to_string(),
                arguments: "not json".to_string(),
            },
        }]);
        let req = ChatRequest::new("gemini-2.5-flash", vec![m]);
        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["args"],
            serde_json::json!({})
        );
    }

    #[test]
    fn to_gemini_request_maps_tools_and_generation_config() {
        let mut req = ChatRequest::new("gemini-2.5-flash", vec![ChatMessage::user("hi")]);
        req.temperature = Some(0.7);
        req.max_tokens = Some(256);
        req.top_p = Some(0.9);
        req.stop = Some(vec!["END".to_string()]);
        req.tools = Some(vec![ToolDefinition {
            name: "exec".to_string(),
            description: "run a command".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }]);

        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        let temperature = body["generationConfig"]["temperature"].as_f64().unwrap();
        assert!(
            (temperature - 0.7).abs() < 1e-6,
            "temperature = {temperature}"
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);
        let top_p = body["generationConfig"]["topP"].as_f64().unwrap();
        assert!((top_p - 0.9).abs() < 1e-6, "top_p = {top_p}");
        assert_eq!(body["generationConfig"]["stopSequences"][0], "END");
        let decl = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "exec");
        assert_eq!(decl["description"], "run a command");
    }

    #[test]
    fn to_gemini_request_omits_generation_config_when_unset() {
        let req = ChatRequest::new("gemini-2.5-flash", vec![ChatMessage::user("hi")]);
        let body = serde_json::to_value(to_gemini_request(&req)).unwrap();
        assert!(body.get("generationConfig").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn new_rejects_empty_api_key() {
        let result = GeminiProvider::new("gemini", None, AuthProfile::api_key(""));
        match result {
            Err(ProviderError::InvalidAuth(_)) => {}
            Err(err) => panic!("expected InvalidAuth, got {err}"),
            Ok(_) => panic!("empty key must be rejected"),
        }
    }

    #[test]
    fn supported_models_includes_static_catalog_and_default_model() {
        let provider = GeminiProvider::new("gemini", None, AuthProfile::api_key("k"))
            .unwrap()
            .with_model("gemini-custom");
        let models = provider.supported_models();
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].id, "gemini-2.5-pro");
        assert_eq!(models[0].context_window, Some(1_000_000));
        assert_eq!(models[0].supports_tool_use, Some(true));
        assert_eq!(models[3].id, "gemini-custom");
    }

    #[test]
    fn supported_models_does_not_duplicate_catalog_default() {
        let provider = GeminiProvider::new("gemini", None, AuthProfile::api_key("k"))
            .unwrap()
            .with_model("gemini-2.5-flash");
        assert_eq!(provider.supported_models().len(), 3);
    }

    #[tokio::test]
    async fn chat_parses_sse_stream_with_tool_calls() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" world\"}]}}]}\r\n\r\n",
            "data: {\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"exec\",\"args\":{\"cmd\":\"ls\"}}}]},\"finishReason\":\"STOP\"}]}\r\n\r\n",
        );
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(
            "gemini",
            Some(server.uri()),
            AuthProfile::api_key("test-key"),
        )
        .unwrap();
        let req = ChatRequest::new("gemini-2.5-flash", vec![ChatMessage::user("hi")]);
        let stream = provider.chat(req).await.unwrap();
        let chunks: Vec<ChatChunk> = stream
            .collect::<Vec<Result<ChatChunk, ProviderError>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].delta, "Hello");
        assert_eq!(chunks[1].delta, " world");
        let last = &chunks[2];
        assert_eq!(last.finish_reason, Some(FinishReason::Stop));
        let calls = last.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_exec_0");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"cmd":"ls"}"#);
    }

    #[tokio::test]
    async fn chat_maps_finish_reasons() {
        let server = MockServer::start().await;
        let sse_body = "data: {\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"x\"}]},\"finishReason\":\"MAX_TOKENS\"}]}\r\n\r\n";
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&server)
            .await;

        let provider =
            GeminiProvider::new("gemini", Some(server.uri()), AuthProfile::api_key("k")).unwrap();
        let req = ChatRequest::new("gemini-2.5-flash", vec![ChatMessage::user("hi")]);
        let stream = provider.chat(req).await.unwrap();
        let chunks: Vec<ChatChunk> = stream
            .collect::<Vec<Result<ChatChunk, ProviderError>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(chunks[0].finish_reason, Some(FinishReason::Length));
    }

    #[tokio::test]
    async fn chat_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let provider =
            GeminiProvider::new("gemini", Some(server.uri()), AuthProfile::api_key("k")).unwrap();
        let req = ChatRequest::new("gemini-2.5-flash", vec![ChatMessage::user("hi")]);
        match provider.chat(req).await {
            Err(ProviderError::StreamAborted(_)) => {}
            Err(err) => panic!("expected StreamAborted, got {err}"),
            Ok(_) => panic!("403 must fail"),
        }
    }

    #[tokio::test]
    async fn embed_parses_batch_embed_contents() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/text-embedding-004:batchEmbedContents"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embeddings": [
                    { "values": [0.1, 0.2] },
                    { "values": [0.3, 0.4] }
                ]
            })))
            .mount(&server)
            .await;

        let provider =
            GeminiProvider::new("gemini", Some(server.uri()), AuthProfile::api_key("k")).unwrap();
        let req = EmbedRequest {
            model: "text-embedding-004".to_string(),
            input: vec!["a".to_string(), "b".to_string()],
            extra: HashMap::new(),
        };
        let embeddings = provider.embed(req).await.unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].index, 0);
        assert_eq!(embeddings[0].embedding, vec![0.1, 0.2]);
        assert_eq!(embeddings[1].index, 1);
        assert_eq!(embeddings[1].embedding, vec![0.3, 0.4]);
    }
}
