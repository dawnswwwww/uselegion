use crate::auth::AuthProfile;
use crate::http;
use crate::provider::Provider;
use crate::types::{
    ChatChunk, ChatRequest, ChatStream, EmbedRequest, Embedding, FinishReason, FunctionCall,
    ModelInfo, ProviderError, ToolCall, role_str,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;

const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Native provider for a local Ollama server.
///
/// Uses `/api/chat` (NDJSON streaming), `/api/embed` for embeddings, and
/// `/api/tags` for model discovery. Ollama is local and unauthenticated, so
/// the auth profile is accepted for signature symmetry but ignored.
pub struct OllamaProvider {
    id: String,
    base_url: String,
    client: reqwest::Client,
    default_model: Option<String>,
}

impl OllamaProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: Option<String>,
        _auth: AuthProfile,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            id: id.into(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string()),
            client: reqwest::Client::new(),
            default_model: None,
        })
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }

    fn embed_url(&self) -> String {
        format!("{}/api/embed", self.base_url.trim_end_matches('/'))
    }

    fn tags_url(&self) -> String {
        format!("{}/api/tags", self.base_url.trim_end_matches('/'))
    }

    fn build_body(&self, req: &ChatRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| {
                let mut obj = serde_json::json!({
                    "role": role_str(&m.role),
                    "content": m.content,
                });
                if let Some(tool_calls) = &m.tool_calls {
                    obj["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_default();
                }
                obj
            })
            .collect();

        let mut options = serde_json::Map::new();
        if let Some(t) = req.temperature {
            options.insert("temperature".to_string(), serde_json::Value::from(t));
        }
        if let Some(p) = req.top_p {
            options.insert("top_p".to_string(), serde_json::Value::from(p));
        }
        if let Some(max) = req.max_tokens {
            options.insert("num_predict".to_string(), serde_json::Value::from(max));
        }
        if let Some(stop) = &req.stop {
            options.insert(
                "stop".to_string(),
                serde_json::to_value(stop).unwrap_or_default(),
            );
        }

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "options": options,
        });

        if let Some(tools) = &req.tools {
            let ollama_tools: Vec<serde_json::Value> = tools
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
            body["tools"] = serde_json::Value::Array(ollama_tools);
        }
        req.apply_extra_to(&mut body);

        body
    }

    /// Discover models installed on the Ollama server via `GET /api/tags`.
    ///
    /// `Provider::supported_models` is synchronous and cannot perform this
    /// request, so model discovery is exposed here for CLI use.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let response = self
            .client
            .get(self.tags_url())
            .send()
            .await?
            .error_for_status()?;
        let payload: OllamaTagsResponse = response.json().await?;
        Ok(payload
            .models
            .into_iter()
            .map(|m| ModelInfo::new(m.name, &self.id))
            .collect())
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn supported_models(&self) -> Vec<ModelInfo> {
        // The trait method is synchronous, so only the configured default
        // model can be advertised here; use `list_models` for live discovery.
        self.default_model
            .as_ref()
            .map(|m| vec![ModelInfo::new(m, &self.id)])
            .unwrap_or_default()
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let body = self.build_body(&req);
        tracing::debug!(url = %self.chat_url(), body = %serde_json::to_string(&body).unwrap_or_default(), "Ollama chat request");

        let response = self.client.post(self.chat_url()).json(&body).send().await?;
        let response = http::check_status(response, false).await?;

        // Ollama streams newline-delimited JSON (not SSE): each line is a
        // complete JSON object. Buffer bytes, split on newlines, and flush a
        // trailing unterminated line at EOF.
        let byte_stream = Box::pin(response.bytes_stream());
        let stream = futures::stream::unfold(
            (byte_stream, String::new(), false),
            |(mut bytes, mut buf, mut done_seen)| async move {
                loop {
                    if done_seen {
                        return None;
                    }
                    if let Some(line) = take_ndjson_line(&mut buf) {
                        match parse_ollama_line_full(&line) {
                            Some(Ok((chunk, done))) => {
                                done_seen = done;
                                return Some((Ok(chunk), (bytes, buf, done_seen)));
                            }
                            Some(Err(err)) => {
                                return Some((Err(err), (bytes, buf, done_seen)));
                            }
                            None => continue,
                        }
                    }
                    match bytes.next().await {
                        Some(Ok(chunk)) => {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                        }
                        Some(Err(err)) => {
                            return Some((Err(ProviderError::Http(err)), (bytes, buf, done_seen)));
                        }
                        None => {
                            if buf.trim().is_empty() {
                                return None;
                            }
                            let line = std::mem::take(&mut buf);
                            return match parse_ollama_line_full(&line) {
                                Some(Ok((chunk, done))) => Some((Ok(chunk), (bytes, buf, done))),
                                Some(Err(err)) => Some((Err(err), (bytes, buf, true))),
                                None => None,
                            };
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
        });

        let response = self
            .client
            .post(self.embed_url())
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let payload: OllamaEmbedResponse = response.json().await?;
        Ok(payload
            .embeddings
            .into_iter()
            .enumerate()
            .map(|(index, embedding)| Embedding { index, embedding })
            .collect())
    }
}

/// Take the next complete newline-terminated line out of `buf`, if any,
/// leaving a trailing partial line buffered until its newline arrives.
///
/// A pure helper so the NDJSON framing can be unit-tested with arbitrary
/// byte-chunk boundaries (wiremock always delivers the body contiguously).
fn take_ndjson_line(buf: &mut String) -> Option<String> {
    let pos = buf.find('\n')?;
    let line = buf[..pos].to_string();
    buf.drain(..=pos);
    Some(line)
}

/// Parse one NDJSON line from the Ollama chat stream.
///
/// Returns `None` for blank lines, `Some(Err(..))` for malformed JSON, and
/// `Some(Ok(chunk))` otherwise. This is a pure function so the line-level
/// mapping can be unit-tested without any I/O.
#[cfg(test)]
fn parse_ollama_line(line: &str) -> Option<Result<ChatChunk, ProviderError>> {
    parse_ollama_line_full(line).map(|r| r.map(|(chunk, _)| chunk))
}

/// Like [`parse_ollama_line`] but also reports whether the line carried
/// `done: true`, so the stream can terminate early.
fn parse_ollama_line_full(line: &str) -> Option<Result<(ChatChunk, bool), ProviderError>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let parsed: OllamaChatLine = match serde_json::from_str(line) {
        Ok(parsed) => parsed,
        Err(err) => return Some(Err(ProviderError::JsonParse(err))),
    };

    let finish_reason = parsed.done_reason.and_then(|r| match r.as_str() {
        "stop" => Some(FinishReason::Stop),
        "length" => Some(FinishReason::Length),
        _ => None,
    });

    let calls: Vec<ToolCall> = parsed
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, call)| {
            let arguments = match call.function.arguments {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            ToolCall {
                id: format!("call_{i}"),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: call.function.name,
                    arguments,
                },
            }
        })
        .collect();

    Some(Ok((
        ChatChunk {
            index: 0,
            delta: parsed.message.content,
            finish_reason,
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
        },
        parsed.done,
    )))
}

#[derive(Debug, Deserialize)]
struct OllamaChatLine {
    #[serde(default)]
    message: OllamaMessage,
    #[serde(default)]
    done: bool,
    done_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OllamaMessage {
    #[serde(default)]
    content: String,
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    function: OllamaFunction,
}

#[derive(Debug, Deserialize)]
struct OllamaFunction {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaTagModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagModel {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ChatRole};
    use std::collections::HashMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_ollama_line_returns_none_for_blank_lines() {
        assert!(parse_ollama_line("").is_none());
        assert!(parse_ollama_line("   ").is_none());
        assert!(parse_ollama_line("\r").is_none());
    }

    #[test]
    fn parse_ollama_line_parses_content_delta() {
        let chunk = parse_ollama_line(
            r#"{"model":"llama3","message":{"role":"assistant","content":"Hello"},"done":false}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(chunk.delta, "Hello");
        assert_eq!(chunk.finish_reason, None);
        assert!(chunk.tool_calls.is_none());
    }

    #[test]
    fn parse_ollama_line_parses_done_with_reason() {
        let chunk = parse_ollama_line(
            r#"{"model":"llama3","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop"}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(chunk.finish_reason, Some(FinishReason::Stop));

        let length = parse_ollama_line(
            r#"{"model":"llama3","message":{"role":"assistant","content":""},"done":true,"done_reason":"length"}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(length.finish_reason, Some(FinishReason::Length));
    }

    #[test]
    fn parse_ollama_line_parses_tool_calls() {
        let chunk = parse_ollama_line(
            r#"{"model":"llama3","message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"exec","arguments":{"cmd":"ls"}}}]},"done":false}"#,
        )
        .unwrap()
        .unwrap();
        let calls = chunk.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"cmd":"ls"}"#);
    }

    #[test]
    fn parse_ollama_line_rejects_malformed_json() {
        let err = parse_ollama_line("not json")
            .unwrap()
            .expect_err("bad line");
        assert!(matches!(err, ProviderError::JsonParse(_)));
    }

    #[test]
    fn take_ndjson_line_handles_lines_split_across_byte_chunks() {
        // Simulate NDJSON bytes arriving in chunks that do not align with
        // line boundaries: a partial line must stay buffered until its
        // newline arrives.
        let mut buf = String::new();
        buf.push_str("{\"a\":1}\n{\"b\":2");
        assert_eq!(take_ndjson_line(&mut buf).as_deref(), Some("{\"a\":1}"));
        assert_eq!(take_ndjson_line(&mut buf), None);
        assert_eq!(buf, "{\"b\":2");

        buf.push_str("}\n{\"c\":3}\n{\"d\":4}");
        assert_eq!(take_ndjson_line(&mut buf).as_deref(), Some("{\"b\":2}"));
        assert_eq!(take_ndjson_line(&mut buf).as_deref(), Some("{\"c\":3}"));
        assert_eq!(take_ndjson_line(&mut buf), None);
        assert_eq!(buf, "{\"d\":4}");
    }

    #[test]
    fn build_body_maps_options_and_tools() {
        let provider = OllamaProvider::new("ollama", None, AuthProfile::api_key(""))
            .unwrap()
            .with_model("llama3");
        let mut req = ChatRequest::new("llama3", vec![ChatMessage::user("hi")]);
        req.temperature = Some(0.5);
        req.max_tokens = Some(128);
        req.stop = Some(vec!["END".to_string()]);
        req.tools = Some(vec![crate::types::ToolDefinition {
            name: "exec".to_string(),
            description: "run".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }]);

        let body = provider.build_body(&req);
        assert_eq!(body["model"], "llama3");
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["temperature"], 0.5);
        assert_eq!(body["options"]["num_predict"], 128);
        assert_eq!(body["options"]["stop"][0], "END");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "exec");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn build_body_maps_tool_role() {
        let provider = OllamaProvider::new("ollama", None, AuthProfile::api_key("")).unwrap();
        let req = ChatRequest::new(
            "llama3",
            vec![ChatMessage {
                role: ChatRole::Tool,
                content: "out".to_string(),
                name: None,
                tool_calls: None,
                tool_call_id: Some("call_0".to_string()),
                cache_breakpoint: false,
            }],
        );
        let body = provider.build_body(&req);
        assert_eq!(body["messages"][0]["role"], "tool");
    }

    #[test]
    fn supported_models_returns_default_model_only() {
        let provider = OllamaProvider::new("ollama", None, AuthProfile::api_key("")).unwrap();
        assert!(provider.supported_models().is_empty());

        let provider = provider.with_model("llama3");
        let models = provider.supported_models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama3");
    }

    #[tokio::test]
    async fn chat_parses_ndjson_stream() {
        let server = MockServer::start().await;
        let ndjson = concat!(
            "{\"model\":\"llama3\",\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"done\":false}\n",
            "{\"model\":\"llama3\",\"message\":{\"role\":\"assistant\",\"content\":\" world\"},\"done\":false}\n",
            "{\"model\":\"llama3\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"exec\",\"arguments\":{\"cmd\":\"ls\"}}}]},\"done\":true,\"done_reason\":\"stop\"}\n",
        );
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ndjson))
            .mount(&server)
            .await;

        let provider =
            OllamaProvider::new("ollama", Some(server.uri()), AuthProfile::api_key("")).unwrap();
        let req = ChatRequest::new("llama3", vec![ChatMessage::user("hi")]);
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
        assert_eq!(calls[0].function.name, "exec");
        assert_eq!(calls[0].function.arguments, r#"{"cmd":"ls"}"#);
    }

    #[tokio::test]
    async fn chat_flushes_final_line_without_trailing_newline() {
        let server = MockServer::start().await;
        // No trailing newline: the final line must be flushed at EOF.
        let ndjson = "{\"model\":\"llama3\",\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"done\":true,\"done_reason\":\"stop\"}";
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ndjson))
            .mount(&server)
            .await;

        let provider =
            OllamaProvider::new("ollama", Some(server.uri()), AuthProfile::api_key("")).unwrap();
        let req = ChatRequest::new("llama3", vec![ChatMessage::user("hi")]);
        let stream = provider.chat(req).await.unwrap();
        let chunks: Vec<ChatChunk> = stream
            .collect::<Vec<Result<ChatChunk, ProviderError>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].delta, "hi");
        assert_eq!(chunks[0].finish_reason, Some(FinishReason::Stop));
    }

    #[tokio::test]
    async fn chat_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404).set_body_string("model not found"))
            .mount(&server)
            .await;

        let provider =
            OllamaProvider::new("ollama", Some(server.uri()), AuthProfile::api_key("")).unwrap();
        let req = ChatRequest::new("llama3", vec![ChatMessage::user("hi")]);
        match provider.chat(req).await {
            Err(ProviderError::StreamAborted(_)) => {}
            Err(err) => panic!("expected StreamAborted, got {err}"),
            Ok(_) => panic!("404 must fail"),
        }
    }

    #[tokio::test]
    async fn embed_parses_embeddings() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "nomic-embed-text",
                "embeddings": [[0.1, 0.2], [0.3, 0.4]]
            })))
            .mount(&server)
            .await;

        let provider =
            OllamaProvider::new("ollama", Some(server.uri()), AuthProfile::api_key("")).unwrap();
        let req = EmbedRequest {
            model: "nomic-embed-text".to_string(),
            input: vec!["a".to_string(), "b".to_string()],
            extra: HashMap::new(),
        };
        let embeddings = provider.embed(req).await.unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].index, 0);
        assert_eq!(embeddings[0].embedding, vec![0.1, 0.2]);
        assert_eq!(embeddings[1].index, 1);
    }

    #[tokio::test]
    async fn list_models_parses_api_tags() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "name": "llama3:latest", "model": "llama3:latest", "size": 1 },
                    { "name": "nomic-embed-text:latest", "model": "nomic-embed-text:latest", "size": 2 }
                ]
            })))
            .mount(&server)
            .await;

        let provider =
            OllamaProvider::new("ollama", Some(server.uri()), AuthProfile::api_key("")).unwrap();
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3:latest");
        assert_eq!(models[0].provider, "ollama");
        assert_eq!(models[1].id, "nomic-embed-text:latest");
    }
}
