use crate::types::{
    ChatRequest, ChatStream, EmbedRequest, Embedding, ImageRequest, ImageResponse, ModelInfo,
    ProviderError, SpeechRequest, SpeechResponse,
};
use async_trait::async_trait;

/// Abstract interface for an LLM provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Unique provider id, e.g. `openai` or `anthropic`.
    fn id(&self) -> &str;

    /// Models advertised by this provider.
    fn supported_models(&self) -> Vec<ModelInfo>;

    /// Start a streaming chat completion.
    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError>;

    /// Generate embeddings for one or more inputs.
    async fn embed(&self, req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError>;

    /// Generate one or more images from a text prompt (tools-p1p2 Phase B).
    ///
    /// Default implementation reports the capability as unsupported, so
    /// existing providers are unaffected; only providers with an image
    /// endpoint override this.
    async fn generate_image(&self, _req: ImageRequest) -> Result<ImageResponse, ProviderError> {
        Err(ProviderError::ImageNotSupported(self.id().to_string()))
    }

    /// Synthesize speech from text (tools-p1p2 Phase C).
    ///
    /// Default implementation reports the capability as unsupported, so
    /// existing providers are unaffected; only providers with a speech
    /// endpoint override this.
    async fn synthesize_speech(
        &self,
        _req: SpeechRequest,
    ) -> Result<SpeechResponse, ProviderError> {
        Err(ProviderError::SpeechNotSupported(self.id().to_string()))
    }
}
