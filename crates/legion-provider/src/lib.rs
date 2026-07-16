pub mod anthropic;
pub mod auth;
pub mod bedrock;
mod eventstream;
pub mod gemini;
pub mod model_ref;
pub mod ollama;
pub mod openai;
pub mod ops;
pub mod provider;
pub mod router;
mod sigv4;
pub mod types;

pub use auth::{AuthProfile, load_auth_profiles};
pub use ops::{CostTracker, RateLimiter, RetryPolicy};
pub use provider::Provider;
pub use router::ProviderRouter;
pub use types::{
    ChatChunk, ChatMessage, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding,
    FinishReason, ModelInfo, ProviderError, ResolvedModelRef, ToolCall, ToolDefinition,
};
