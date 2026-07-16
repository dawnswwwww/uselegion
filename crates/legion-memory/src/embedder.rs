use async_trait::async_trait;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{EmbedRequest, ProviderError};
use std::sync::Arc;

use crate::MemoryError;

/// Pluggable source of dense embeddings.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts.
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, MemoryError>;

    /// Vector dimension produced by this embedder.
    fn dimension(&self) -> usize;
}

/// Deterministic embedder for tests.
///
/// Produces unit-normalised vectors from a hash of the input text so that
/// repeated indexing of the same text yields identical embeddings.
pub struct FakeEmbedder {
    dimension: usize,
}

impl FakeEmbedder {
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut vec = vec![0.0f32; self.dimension];

        // Build a bag-of-words embedding so texts sharing words get similar vectors.
        for word in text.split_whitespace() {
            let word = word.to_lowercase();
            let mut hasher = DefaultHasher::new();
            word.hash(&mut hasher);
            let seed = hasher.finish();
            let mut x = seed;
            for component in &mut vec {
                // xorshift PRNG step.
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *component += (x as f32) / (u64::MAX as f32);
            }
        }

        // If no words, fall back to hashing the full text.
        if vec.iter().all(|&v| v == 0.0) {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            let seed = hasher.finish();
            let mut x = seed;
            for component in &mut vec {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *component = (x as f32) / (u64::MAX as f32);
            }
        }

        // Normalise to unit length for stable cosine similarity.
        let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
        let norm = norm_sq.sqrt().max(f32::EPSILON);
        for v in &mut vec {
            *v /= norm;
        }
        vec
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, MemoryError> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}

/// Embedder that delegates to `legion-provider`.
pub struct ProviderEmbedder {
    router: Arc<ProviderRouter>,
    model_ref: String,
    dimension: usize,
}

impl ProviderEmbedder {
    pub fn new(
        router: Arc<ProviderRouter>,
        model_ref: impl Into<String>,
        dimension: usize,
    ) -> Self {
        Self {
            router,
            model_ref: model_ref.into(),
            dimension,
        }
    }
}

#[async_trait]
impl Embedder for ProviderEmbedder {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, MemoryError> {
        let req = EmbedRequest {
            model: String::new(),
            input: texts,
            extra: Default::default(),
        };
        let embeddings = self
            .router
            .embed(&self.model_ref, req)
            .await
            .map_err(provider_err_to_memory_err)?;
        Ok(embeddings.into_iter().map(|e| e.embedding).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}

fn provider_err_to_memory_err(err: ProviderError) -> MemoryError {
    MemoryError::SearchFailed(format!("embedding provider failed: {err}"))
}
