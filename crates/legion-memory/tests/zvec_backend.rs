use legion_core::config::{DecayConfig, MergeConfig};
use legion_memory::{FakeEmbedder, MemoryBackend, MemoryMeta, SqliteVecBackend};
use std::sync::Arc;
use tempfile::TempDir;

async fn make_backend() -> (TempDir, TempDir, SqliteVecBackend) {
    let collection = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let embedder = Arc::new(FakeEmbedder::new(16));
    let backend = SqliteVecBackend::open(collection.path(), workspace.path(), embedder)
        .await
        .unwrap();
    (collection, workspace, backend)
}

#[tokio::test]
async fn should_find_relevant_memory_by_vector() {
    let (_collection, _workspace, backend) = make_backend().await;

    backend
        .index(
            "doc1",
            "I love Rust programming language",
            MemoryMeta::default(),
        )
        .await
        .unwrap();
    backend
        .index(
            "doc2",
            "Python is a nice language for beginners",
            MemoryMeta::default(),
        )
        .await
        .unwrap();

    let results = backend
        .search("favorite programming language rust", 2)
        .await
        .unwrap();

    assert!(!results.is_empty());
    // The Rust doc should rank higher because it shares more words with the query.
    assert_eq!(results[0].id, "doc1");
}

#[tokio::test]
async fn should_find_relevant_memory_by_keyword() {
    let (_collection, _workspace, backend) = make_backend().await;

    backend
        .index("doc1", "The quick brown fox", MemoryMeta::default())
        .await
        .unwrap();
    backend
        .index("doc2", "Lazy dogs sleep all day", MemoryMeta::default())
        .await
        .unwrap();

    let results = backend.search("fox", 2).await.unwrap();

    // Hybrid search should surface the keyword match first.
    assert_eq!(results[0].id, "doc1");
}

#[tokio::test]
async fn should_hybrid_search_combine_vector_and_keyword() {
    let (_collection, _workspace, backend) = make_backend().await;

    backend
        .index("doc1", "Rust async runtime overview", MemoryMeta::default())
        .await
        .unwrap();
    backend
        .index("doc2", "Python asyncio event loop", MemoryMeta::default())
        .await
        .unwrap();

    let results = backend.search("async runtime", 2).await.unwrap();

    assert_eq!(results.len(), 2);
    // doc1 contains both keywords, doc2 only one.
    assert_eq!(results[0].id, "doc1");
}

#[tokio::test]
async fn should_get_file_content_with_line_range() {
    let (_collection, workspace, backend) = make_backend().await;

    let content = "line one\nline two\nline three\nline four";
    tokio::fs::write(workspace.path().join("MEMORY.md"), content)
        .await
        .unwrap();

    let full = backend.get("MEMORY.md", None).await.unwrap();
    assert_eq!(full, content);

    let range = backend.get("MEMORY.md", Some(1..3)).await.unwrap();
    assert_eq!(range, "line two\nline three");
}

#[tokio::test]
async fn should_persist_after_reopen() {
    let (collection, workspace, backend) = make_backend().await;

    backend
        .index(
            "persisted",
            "persistent memory content",
            MemoryMeta::default(),
        )
        .await
        .unwrap();

    // Drop the backend, then reopen the same collection.
    drop(backend);

    let embedder = Arc::new(FakeEmbedder::new(16));
    let backend = SqliteVecBackend::open(collection.path(), workspace.path(), embedder)
        .await
        .unwrap();

    let results = backend.search("persistent", 1).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "persisted");
    assert!(results[0].content.contains("persistent"));
}

#[tokio::test]
async fn should_index_file_and_read_back() {
    let (_collection, workspace, backend) = make_backend().await;

    let memory_dir = workspace.path().join("memory");
    tokio::fs::create_dir_all(&memory_dir).await.unwrap();
    tokio::fs::write(
        memory_dir.join("2026-07-08.md"),
        "Today I learned about Rust lifetimes.",
    )
    .await
    .unwrap();

    backend
        .index_file(std::path::Path::new("memory/2026-07-08.md"))
        .await
        .unwrap();

    let results = backend.search("Rust lifetimes", 1).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "memory/2026-07-08.md");
}

#[tokio::test]
async fn merge_drops_duplicate_episodic_keeps_newest() {
    let collection = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let embedder = Arc::new(FakeEmbedder::new(16));
    let backend = SqliteVecBackend::open(collection.path(), workspace.path(), embedder)
        .await
        .unwrap()
        .with_merge_config(MergeConfig {
            enabled: true,
            model: None,
            similarity_threshold: 0.99,
            max_candidates: 200,
        });

    let meta = MemoryMeta {
        kind: Some("episodic".into()),
        ..Default::default()
    };
    backend
        .index("e1", "User prefers Rust for systems work", meta.clone())
        .await
        .unwrap();
    backend
        .index("e2", "User prefers Rust for systems work", meta.clone())
        .await
        .unwrap();
    backend
        .index("e3", "Gardening tips for spring tomatoes", meta)
        .await
        .unwrap();

    let report = backend.decay_and_merge().await.unwrap();
    assert_eq!(report.merged, 1);
    assert_eq!(report.dropped, 1);

    let dup = backend
        .search("User prefers Rust for systems work", 10)
        .await
        .unwrap();
    let ids: Vec<_> = dup.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&"e2"), "newer duplicate kept: {ids:?}");
    assert!(!ids.contains(&"e1"), "older duplicate dropped: {ids:?}");

    let other = backend.search("gardening tomatoes", 10).await.unwrap();
    assert!(other.iter().any(|n| n.id == "e3"), "distinct note survives");
}

#[tokio::test]
async fn merge_disabled_is_noop() {
    let (_collection, _workspace, backend) = make_backend().await;
    let meta = MemoryMeta {
        kind: Some("episodic".into()),
        ..Default::default()
    };
    backend
        .index("e1", "same content here", meta.clone())
        .await
        .unwrap();
    backend
        .index("e2", "same content here", meta)
        .await
        .unwrap();

    let report = backend.decay_and_merge().await.unwrap();
    assert_eq!(report.dropped, 0);
    assert_eq!(report.merged, 0);
}

#[tokio::test]
async fn decay_enabled_keeps_fresh_episodic_scores() {
    let collection = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let embedder = Arc::new(FakeEmbedder::new(16));
    let backend = SqliteVecBackend::open(collection.path(), workspace.path(), embedder)
        .await
        .unwrap()
        .with_decay_config(DecayConfig {
            enabled: true,
            half_life_days: 30.0,
        });

    let meta = MemoryMeta {
        kind: Some("episodic".into()),
        ..Default::default()
    };
    backend
        .index("e1", "User prefers Rust", meta.clone())
        .await
        .unwrap();
    backend
        .index("e2", "User prefers Rust", meta)
        .await
        .unwrap();

    // Fresh notes have age ~0 -> factor ~1, so equal-content notes keep equal score.
    let results = backend.search("User prefers Rust", 10).await.unwrap();
    assert!(results.len() >= 2);
    let s1 = results.iter().find(|n| n.id == "e1").unwrap().score;
    let s2 = results.iter().find(|n| n.id == "e2").unwrap().score;
    assert!(
        (s1 - s2).abs() < 1e-4,
        "fresh equal notes should keep equal score: {s1} vs {s2}"
    );
}
