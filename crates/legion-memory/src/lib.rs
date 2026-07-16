//! Legion memory subsystem.
//!
//! Provides a `builtin` memory backend backed by SQLite (structured storage,
//! FTS5 keyword search, and the `sqlite-vec` extension for dense vector search).
//!
//! # Fallback note
//!
//! The PRD targets ZVec as the vector store, but the `zvec` crate (with the
//! `bundled` feature) did not complete its native-library build within the
//! available tooling timeouts. This crate therefore falls back to SQLite + the
//! `sqlite-vec` extension, which satisfies the same functional requirements
//! (dense vector + FTS5 hybrid retrieval, local persistence) while remaining
//! fully embeddable and compilable.

mod backend;
mod embedder;

pub use backend::SqliteVecBackend;
pub use embedder::{Embedder, FakeEmbedder, ProviderEmbedder};
pub use legion_runtime::memory::MemoryMeta;

pub use legion_runtime::memory::{
    MemoryBackend, MemoryError, MemoryKind, MemoryNote, RecallContext,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_embedder_produces_deterministic_vectors() {
        let embedder = FakeEmbedder::new(8);
        let a = embedder.embed(vec!["hello".to_string()]).await.unwrap();
        let b = embedder.embed(vec!["hello".to_string()]).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a[0].len(), 8);
    }

    #[tokio::test]
    async fn fake_embedder_normalises_vectors() {
        let embedder = FakeEmbedder::new(16);
        let vecs = embedder.embed(vec!["Rust".to_string()]).await.unwrap();
        let v = &vecs[0];
        let norm_sq: f32 = v.iter().map(|x| x * x).sum();
        assert!((norm_sq - 1.0).abs() < 1e-6, "vector should be unit length");
    }
}
