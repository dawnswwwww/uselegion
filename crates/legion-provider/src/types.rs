use futures::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use thiserror::Error;

/// Errors that can be returned by a provider.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("SSE parse error: {0}")]
    SseParse(String),
    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),
    #[error("provider '{0}' not found")]
    ProviderNotFound(String),
    #[error("model '{0}' not supported by provider '{1}'")]
    ModelNotSupported(String, String),
    #[error("auth profile '{0}' not found")]
    AuthProfileNotFound(String),
    #[error("invalid model reference: {0}")]
    InvalidModelRef(String),
    #[error("invalid auth configuration: {0}")]
    InvalidAuth(String),
    #[error("streaming aborted: {0}")]
    StreamAborted(String),
    #[error("embedding not supported by provider '{0}'")]
    EmbeddingNotSupported(String),
    #[error("all providers in fallback chain failed")]
    AllProvidersFailed,
    #[error("prompt too long for model context window")]
    PromptTooLong,
    #[error("rate limit exceeded for provider '{0}' (wait budget exhausted)")]
    RateLimited(String),
    #[error("provider call timed out: {0}")]
    Timeout(String),
    #[error("image generation not supported by provider '{0}'")]
    ImageNotSupported(String),
    #[error("speech synthesis not supported by provider '{0}'")]
    SpeechNotSupported(String),
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Role of a chat message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single chat message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Marker for providers that support prompt caching (e.g. Anthropic).
    /// When true, the provider may mark this message as a cache breakpoint.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cache_breakpoint: bool,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            cache_breakpoint: false,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            cache_breakpoint: false,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            cache_breakpoint: false,
        }
    }

    /// Return a copy of this message with `cache_breakpoint` set to `true`.
    pub fn with_cache_breakpoint(mut self) -> Self {
        self.cache_breakpoint = true;
        self
    }
}

/// A tool call emitted by an assistant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

/// Function call payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            stream: Some(true),
            tools: None,
            extra: HashMap::new(),
        }
    }
}

/// Tool definition for provider tool-use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A streamed chunk from a chat completion.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatChunk {
    pub index: usize,
    pub delta: String,
    pub finish_reason: Option<FinishReason>,
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Reason a completion finished.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
}

/// A boxed, sendable stream of chat chunks.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk, ProviderError>> + Send>>;

/// Embedding request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedRequest {
    pub model: String,
    pub input: Vec<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A single embedding vector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Embedding {
    pub index: usize,
    pub embedding: Vec<f32>,
}

/// Image generation request (tools-p1p2 Phase B).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
}

/// Image generation response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageResponse {
    pub images: Vec<GeneratedImage>,
}

/// A single generated image: either a hosted URL or base64-encoded PNG data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeneratedImage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
}

/// Speech synthesis request (tools-p1p2 Phase C).
///
/// `model` is filled by the router per fallback candidate; `voice` and
/// `format` are provider-interpreted (e.g. OpenAI voice names, `mp3`/`opus`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRequest {
    pub model: String,
    pub input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

/// Speech synthesis response: raw audio bytes plus the container format
/// actually produced (e.g. `mp3`), used to name the output file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpeechResponse {
    pub audio: Vec<u8>,
    pub format: String,
}

/// Metadata about a model supported by a provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tool_use: Option<bool>,
}

impl ModelInfo {
    pub fn new(id: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            provider: provider.into(),
            context_window: None,
            supports_vision: None,
            supports_tool_use: None,
        }
    }
}

/// Resolved pair of provider id and the model name to pass to that provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModelRef {
    pub provider_id: String,
    pub model_name: String,
}
