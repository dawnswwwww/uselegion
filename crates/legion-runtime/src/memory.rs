use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ops::Range;
use thiserror::Error;

/// A single memory note returned by a memory backend search.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryNote {
    pub id: String,
    pub content: String,
    pub score: f32,
    /// Layer the note belongs to. Drives retrieval weighting in [`MemoryBackend::recall`].
    /// `None` for entries that predate layered memory.
    pub kind: Option<MemoryKind>,
}

/// Memory layer. Retrieval weight decreases from Working to Semantic; Semantic
/// is retained longest (decay handled in a later phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    /// Current-session summary, auto-updated with compaction.
    Working,
    /// Cross-session episodic facts, settled in the background.
    Episodic,
    /// Durable knowledge / preferences / project facts.
    Semantic,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Working => "working",
            MemoryKind::Episodic => "episodic",
            MemoryKind::Semantic => "semantic",
        }
    }

    /// Retrieval weight applied as a multiplier to the base relevance score.
    pub fn weight(&self) -> f32 {
        match self {
            MemoryKind::Working => 1.0,
            MemoryKind::Episodic => 0.75,
            MemoryKind::Semantic => 0.55,
        }
    }
}

impl std::str::FromStr for MemoryKind {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "working" => Ok(MemoryKind::Working),
            "episodic" => Ok(MemoryKind::Episodic),
            "semantic" => Ok(MemoryKind::Semantic),
            _ => Err(()),
        }
    }
}

/// Metadata attached to a memory entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryMeta {
    /// Source file path (e.g. `MEMORY.md`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// ISO-8601 date string (e.g. `2026-07-08`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Entry type / layer label (e.g. `fact`, `preference`, `episodic`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Arbitrary tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl MemoryMeta {
    /// Return the source path, if any.
    pub fn path(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Parse [`MemoryMeta::kind`] into a typed [`MemoryKind`], if it names one.
    pub fn kind_enum(&self) -> Option<MemoryKind> {
        self.kind.as_deref().and_then(|s| s.parse().ok())
    }
}

/// Errors that can occur when interacting with a memory backend.
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory search failed: {0}")]
    SearchFailed(String),

    #[error("memory get failed: {0}")]
    GetFailed(String),

    #[error("memory index failed: {0}")]
    IndexFailed(String),
}

const DEFAULT_RECALL_LIMIT: usize = 5;
const RECALL_OVERFETCH_FACTOR: usize = 3;

/// Controls a [`MemoryBackend::recall`] invocation.
#[derive(Debug, Clone)]
pub struct RecallContext {
    /// Note ids already surfaced to the prompt this session; filtered out.
    pub already_surfaced: HashSet<String>,
    /// Active tool names; notes whose `id` matches a tool are filtered out to
    /// avoid re-injecting tool docs already present in the manifest.
    pub recent_tools: Vec<String>,
    /// Maximum number of notes to return (default 5).
    pub limit: usize,
}

impl Default for RecallContext {
    fn default() -> Self {
        Self {
            already_surfaced: HashSet::new(),
            recent_tools: Vec::new(),
            limit: DEFAULT_RECALL_LIMIT,
        }
    }
}

/// Outcome of a [`MemoryBackend::decay_and_merge`] run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecayReport {
    /// Number of duplicate groups that were merged.
    pub merged: usize,
    /// Number of entries removed as duplicates.
    pub dropped: usize,
}

/// Abstract memory backend.
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    /// Semantic/keyword hybrid search over the agent's memory collection.
    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<MemoryNote>, MemoryError>;

    /// Read a memory file (or a line range from it).
    async fn get(&self, path: &str, range: Option<Range<usize>>) -> Result<String, MemoryError>;

    /// Index (add or update) a single memory entry.
    async fn index(&self, id: &str, content: &str, meta: MemoryMeta) -> Result<(), MemoryError>;

    /// Recall relevant notes for `query`: over-fetch, reweight by [`MemoryKind`],
    /// drop already-surfaced and active-tool notes, then return the top `limit`.
    ///
    /// Implemented as a default method so backends only need to provide
    /// [`MemoryBackend::search`] with [`MemoryNote::kind`] populated.
    async fn recall(
        &self,
        query: &str,
        ctx: &RecallContext,
    ) -> Result<Vec<MemoryNote>, MemoryError> {
        let limit = ctx.limit.max(1);
        let overfetch = limit.saturating_mul(RECALL_OVERFETCH_FACTOR).max(limit);
        let mut notes = self.search(query, overfetch).await?;

        for note in &mut notes {
            let weight = note.kind.map(|k| k.weight()).unwrap_or(1.0);
            note.score *= weight;
        }

        notes.retain(|note| {
            if ctx.already_surfaced.contains(&note.id) {
                return false;
            }
            if ctx.recent_tools.iter().any(|tool| tool == &note.id) {
                return false;
            }
            true
        });

        notes.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        notes.truncate(limit);
        Ok(notes)
    }

    /// Merge near-duplicate entries and drop stale ones. Default no-op; backends
    /// that support maintenance (e.g. sqlite-vec) override it.
    async fn decay_and_merge(&self) -> Result<DecayReport, MemoryError> {
        Ok(DecayReport::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_kind_round_trips_str() {
        for kind in [
            MemoryKind::Working,
            MemoryKind::Episodic,
            MemoryKind::Semantic,
        ] {
            assert_eq!(kind.as_str().parse::<MemoryKind>().ok(), Some(kind));
        }
        assert_eq!("unknown".parse::<MemoryKind>().ok(), None);
    }

    #[test]
    fn memory_kind_weight_order() {
        assert!(MemoryKind::Working.weight() > MemoryKind::Episodic.weight());
        assert!(MemoryKind::Episodic.weight() > MemoryKind::Semantic.weight());
    }

    #[test]
    fn meta_kind_enum_parses() {
        let meta = MemoryMeta {
            kind: Some("episodic".to_string()),
            ..Default::default()
        };
        assert_eq!(meta.kind_enum(), Some(MemoryKind::Episodic));
        let other = MemoryMeta {
            kind: Some("fact".to_string()),
            ..Default::default()
        };
        assert_eq!(other.kind_enum(), None);
    }

    struct FakeBackend {
        notes: Vec<MemoryNote>,
    }

    #[async_trait]
    impl MemoryBackend for FakeBackend {
        async fn search(&self, _query: &str, top_k: usize) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(self.notes.iter().take(top_k).cloned().collect())
        }
        async fn get(
            &self,
            _path: &str,
            _range: Option<Range<usize>>,
        ) -> Result<String, MemoryError> {
            Ok(String::new())
        }
        async fn index(
            &self,
            _id: &str,
            _content: &str,
            _meta: MemoryMeta,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    fn note(id: &str, content: &str, score: f32, kind: Option<MemoryKind>) -> MemoryNote {
        MemoryNote {
            id: id.to_string(),
            content: content.to_string(),
            score,
            kind,
        }
    }

    #[tokio::test]
    async fn recall_dedups_already_surfaced() {
        let backend = FakeBackend {
            notes: vec![
                note("a", "alpha", 0.9, Some(MemoryKind::Episodic)),
                note("b", "beta", 0.8, Some(MemoryKind::Episodic)),
            ],
        };
        let mut surfaced = HashSet::new();
        surfaced.insert("a".to_string());
        let ctx = RecallContext {
            already_surfaced: surfaced,
            recent_tools: Vec::new(),
            limit: 5,
        };
        let out = backend.recall("x", &ctx).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "b");
    }

    #[tokio::test]
    async fn recall_dedups_recent_tools_by_id() {
        let backend = FakeBackend {
            notes: vec![
                note("read", "read tool doc", 0.9, Some(MemoryKind::Semantic)),
                note("fact-1", "user likes rust", 0.8, Some(MemoryKind::Episodic)),
            ],
        };
        let ctx = RecallContext {
            already_surfaced: HashSet::new(),
            recent_tools: vec!["read".to_string()],
            limit: 5,
        };
        let out = backend.recall("x", &ctx).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "fact-1");
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let backend = FakeBackend {
            notes: (0..10)
                .map(|i| note(&format!("n{i}"), "c", 0.5, None))
                .collect(),
        };
        let out = backend
            .recall("x", &RecallContext::default())
            .await
            .unwrap();
        assert_eq!(out.len(), DEFAULT_RECALL_LIMIT);
    }

    #[tokio::test]
    async fn recall_reweights_by_kind() {
        // Semantic has a higher raw score but lower weight; Episodic wins.
        let backend = FakeBackend {
            notes: vec![
                note("sem", "semantic fact", 1.0, Some(MemoryKind::Semantic)), // 0.55
                note("epi", "episodic fact", 0.9, Some(MemoryKind::Episodic)), // 0.675
            ],
        };
        let out = backend
            .recall("x", &RecallContext::default())
            .await
            .unwrap();
        assert_eq!(out[0].id, "epi");
    }
}
