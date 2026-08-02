use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use tracing::{debug, instrument};

use crate::embedder::Embedder;
use crate::{MemoryBackend, MemoryError, MemoryNote};
use legion_core::config::{DecayConfig, MergeConfig};
use legion_runtime::memory::{DecayReport, MemoryMeta};

static LOAD_VEC: Once = Once::new();

/// Ensure the sqlite-vec extension is auto-loaded into every new connection.
fn ensure_vec_extension() {
    LOAD_VEC.call_once(|| unsafe {
        type AutoExtFn = unsafe extern "C" fn(
            *mut libsqlite3_sys::sqlite3,
            *mut *mut std::ffi::c_char,
            *const libsqlite3_sys::sqlite3_api_routines,
        ) -> std::ffi::c_int;
        libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute::<*const (), AutoExtFn>(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// SQLite + sqlite-vec backed memory backend.
///
/// Stores structured documents in SQLite, keyword search in an FTS5 virtual
/// table, and dense vectors in a `vec0` virtual table. Results from the two
/// retrieval paths are fused with reciprocal rank fusion.
pub struct SqliteVecBackend {
    db: Pool<Sqlite>,
    embedder: Arc<dyn Embedder>,
    workspace: PathBuf,
    decay: DecayConfig,
    merge: MergeConfig,
}

impl SqliteVecBackend {
    /// Open (or create) a backend at `collection_path` for the given workspace.
    ///
    /// `collection_path` is the directory that will hold `memory.db`.
    pub async fn open(
        collection_path: impl AsRef<Path>,
        workspace: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self, MemoryError> {
        ensure_vec_extension();

        let collection_path = collection_path.as_ref().to_path_buf();
        std::fs::create_dir_all(&collection_path).map_err(io_err_to_memory_err)?;

        let db_path = collection_path.join("memory.db");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(sqlx_err_to_memory_err)?;

        // Ensure the sqlite-vec extension is loaded in every connection.
        let _ = pool
            .acquire()
            .await
            .map_err(sqlx_err_to_memory_err)?
            .as_mut();

        let backend = Self {
            db: pool,
            embedder,
            workspace: workspace.as_ref().to_path_buf(),
            decay: DecayConfig::default(),
            merge: MergeConfig::default(),
        };
        backend.init_schema().await?;
        Ok(backend)
    }

    /// Attach a decay config (Phase C). Disabled-by-default configs leave ranking
    /// unchanged.
    pub fn with_decay_config(mut self, decay: DecayConfig) -> Self {
        self.decay = decay;
        self
    }

    /// Attach a merge config (Phase C). Disabled-by-default configs make
    /// `decay_and_merge` a no-op.
    pub fn with_merge_config(mut self, merge: MergeConfig) -> Self {
        self.merge = merge;
        self
    }

    async fn init_schema(&self) -> Result<(), MemoryError> {
        let dim = self.embedder.dimension();

        // Documents table.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS documents (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                path TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                meta TEXT NOT NULL DEFAULT '{}'
            )
            "#,
        )
        .execute(&self.db)
        .await
        .map_err(sqlx_err_to_memory_err)?;

        // FTS5 keyword index.
        sqlx::query(
            r#"
            CREATE VIRTUAL TABLE IF NOT EXISTS document_fts USING fts5(
                id UNINDEXED,
                content,
                tokenize='porter'
            )
            "#,
        )
        .execute(&self.db)
        .await
        .map_err(sqlx_err_to_memory_err)?;

        // Dense vector index. Dimension is fixed by the embedder.
        let vec_ddl = format!(
            r#"
            CREATE VIRTUAL TABLE IF NOT EXISTS document_vec USING vec0(
                embedding FLOAT[{dim}]
            )
            "#
        );
        sqlx::query(&vec_ddl)
            .execute(&self.db)
            .await
            .map_err(sqlx_err_to_memory_err)?;

        Ok(())
    }

    /// Index a single memory entry.
    #[instrument(skip(self, content, meta), fields(id = %id))]
    pub async fn index_with_meta(
        &self,
        id: &str,
        content: &str,
        meta: MemoryMeta,
    ) -> Result<(), MemoryError> {
        if content.is_empty() {
            return Ok(());
        }

        let embeddings = self.embedder.embed(vec![content.to_string()]).await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| MemoryError::SearchFailed("empty embedding response".to_string()))?;

        let meta_json = serde_json::to_string(&meta).map_err(json_err_to_memory_err)?;

        let mut tx = self.db.begin().await.map_err(sqlx_err_to_memory_err)?;

        // Upsert documents row.
        let rowid: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO documents (id, content, path, meta)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                path = excluded.path,
                meta = excluded.meta,
                created_at = datetime('now')
            RETURNING rowid
            "#,
        )
        .bind(id)
        .bind(content)
        .bind(meta.path())
        .bind(&meta_json)
        .fetch_one(&mut *tx)
        .await
        .map_err(sqlx_err_to_memory_err)?;

        // Update FTS row.
        sqlx::query("DELETE FROM document_fts WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err_to_memory_err)?;
        sqlx::query("INSERT INTO document_fts (id, content) VALUES (?, ?)")
            .bind(id)
            .bind(content)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err_to_memory_err)?;

        // Update vector row. sqlite-vec lets us use the document rowid directly.
        sqlx::query("DELETE FROM document_vec WHERE rowid = ?")
            .bind(rowid)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err_to_memory_err)?;

        let vec_json = format_vec(&embedding);
        let vec_insert = "INSERT INTO document_vec (rowid, embedding) VALUES (?, ?)";
        sqlx::query(vec_insert)
            .bind(rowid)
            .bind(&vec_json)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err_to_memory_err)?;

        tx.commit().await.map_err(sqlx_err_to_memory_err)?;
        debug!(%id, rowid, "indexed memory entry");
        Ok(())
    }

    /// Read and index a file from the agent workspace.
    pub async fn index_file(&self, path: &Path) -> Result<(), MemoryError> {
        let full_path = self.workspace.join(path);
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .map_err(io_err_to_memory_err)?;

        let id = path.to_string_lossy().to_string();
        let date = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|stem| {
                stem.len() == 10
                    && stem.chars().nth(4) == Some('-')
                    && stem.chars().nth(7) == Some('-')
            })
            .map(|stem| stem.to_string());

        let meta = MemoryMeta {
            source: Some(id.clone()),
            date,
            kind: Some("file".to_string()),
            ..Default::default()
        };

        self.index_with_meta(&id, &content, meta).await
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            self.workspace.join(path)
        }
    }
}

#[async_trait]
impl MemoryBackend for SqliteVecBackend {
    async fn index(&self, id: &str, content: &str, meta: MemoryMeta) -> Result<(), MemoryError> {
        self.index_with_meta(id, content, meta).await
    }

    /// Merge near-duplicate Episodic entries (Phase C). No-op unless `merge.enabled`.
    ///
    /// Groups episodic candidates by cosine similarity (>= `merge.similarity_threshold`)
    /// over re-embedded content, keeps the newest entry of each group, and deletes the
    /// rest. Deterministic (no LLM); LLM synthesis is left for a follow-up.
    #[instrument(skip(self))]
    async fn decay_and_merge(&self) -> Result<DecayReport, MemoryError> {
        if !self.merge.enabled {
            return Ok(DecayReport::default());
        }

        let limit = self.merge.max_candidates as i64;
        let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT rowid, id, content, meta FROM documents \
             ORDER BY created_at DESC, rowid DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_err_to_memory_err)?;

        let mut cands: Vec<(i64, String, String)> = Vec::new(); // (rowid, id, content)
        for (rowid, id, content, meta_json) in rows {
            let is_episodic = meta_json
                .as_deref()
                .and_then(|m| serde_json::from_str::<MemoryMeta>(m).ok())
                .and_then(|meta| meta.kind_enum())
                == Some(legion_runtime::memory::MemoryKind::Episodic);
            if is_episodic && !content.is_empty() {
                cands.push((rowid, id, content));
            }
        }
        if cands.len() < 2 {
            return Ok(DecayReport::default());
        }

        let texts: Vec<String> = cands.iter().map(|c| c.2.clone()).collect();
        let vecs = self.embedder.embed(texts).await?;
        if vecs.len() != cands.len() {
            return Err(MemoryError::SearchFailed(
                "embedding count mismatch during merge".into(),
            ));
        }

        let threshold = self.merge.similarity_threshold;
        let n = cands.len();
        let mut removed = vec![false; n];
        let mut delete: Vec<(i64, String)> = Vec::new();
        let mut merged = 0usize;
        let mut dropped = 0usize;

        for i in 0..n {
            if removed[i] {
                continue;
            }
            let mut group_dropped = 0usize;
            for j in (i + 1)..n {
                if removed[j] {
                    continue;
                }
                if cosine(&vecs[i], &vecs[j]) >= threshold {
                    removed[j] = true;
                    delete.push((cands[j].0, cands[j].1.clone()));
                    group_dropped += 1;
                }
            }
            if group_dropped > 0 {
                merged += 1;
                dropped += group_dropped;
            }
        }

        if !delete.is_empty() {
            let mut tx = self.db.begin().await.map_err(sqlx_err_to_memory_err)?;
            for (rowid, id) in &delete {
                sqlx::query("DELETE FROM document_vec WHERE rowid = ?")
                    .bind(rowid)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_err_to_memory_err)?;
                sqlx::query("DELETE FROM document_fts WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_err_to_memory_err)?;
                sqlx::query("DELETE FROM documents WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_err_to_memory_err)?;
            }
            tx.commit().await.map_err(sqlx_err_to_memory_err)?;
        }

        tracing::info!(merged, dropped, "memory decay_and_merge complete");
        Ok(DecayReport { merged, dropped })
    }

    #[instrument(skip(self), fields(query = %query, top_k = %top_k))]
    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<MemoryNote>, MemoryError> {
        if query.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }

        let query_embedding = self
            .embedder
            .embed(vec![query.to_string()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| MemoryError::SearchFailed("empty query embedding".to_string()))?;

        let top_k_i64 = top_k as i64;

        // Vector search.
        let vec_json = format_vec(&query_embedding);
        let vec_results: Vec<(String, f32)> = sqlx::query(
            r#"
            SELECT d.id, v.distance
            FROM document_vec AS v
            JOIN documents AS d ON d.rowid = v.rowid
            WHERE v.embedding MATCH ? AND v.k = ?
            ORDER BY v.distance
            LIMIT ?
            "#,
        )
        .bind(&vec_json)
        .bind(top_k_i64)
        .bind(top_k_i64)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_err_to_memory_err)?
        .into_iter()
        .map(|row| {
            let id: String = row.try_get("id").unwrap_or_default();
            let dist: f64 = row.try_get("distance").unwrap_or(f64::MAX);
            (id, dist as f32)
        })
        .collect();

        // Keyword search via FTS5.
        let fts_results: Vec<String> = sqlx::query(
            r#"
            SELECT d.id
            FROM document_fts AS f
            JOIN documents AS d ON d.id = f.id
            WHERE document_fts MATCH ?
            ORDER BY rank
            LIMIT ?
            "#,
        )
        .bind(query)
        .bind(top_k_i64)
        .fetch_all(&self.db)
        .await
        .map_err(sqlx_err_to_memory_err)?
        .into_iter()
        .map(|row| row.try_get::<String, _>("id").unwrap_or_default())
        .filter(|id| !id.is_empty())
        .collect();

        // Reciprocal rank fusion.
        let vec_ids: Vec<String> = vec_results.into_iter().map(|(id, _)| id).collect();
        let fused = reciprocal_rank_fusion(vec![vec_ids, fts_results], top_k, 60.0);

        // Fetch content, layer metadata, and age for the fused ids.
        let mut notes = Vec::with_capacity(fused.len());
        for (id, mut score) in fused {
            let row: Option<(String, Option<String>, f64)> = sqlx::query_as(
                "SELECT content, meta, (julianday('now') - julianday(created_at)) \
                 FROM documents WHERE id = ?",
            )
            .bind(&id)
            .fetch_optional(&self.db)
            .await
            .map_err(sqlx_err_to_memory_err)?;

            if let Some((content, meta_json, age_days)) = row {
                let kind = meta_json
                    .as_deref()
                    .and_then(|m| serde_json::from_str::<MemoryMeta>(m).ok())
                    .and_then(|meta| meta.kind_enum());
                if self.decay.enabled && kind == Some(legion_runtime::memory::MemoryKind::Episodic)
                {
                    score *= decay_factor(age_days as f32, self.decay.half_life_days);
                }
                notes.push(MemoryNote {
                    id,
                    content,
                    score,
                    kind,
                });
            }
        }

        Ok(notes)
    }

    #[instrument(skip(self), fields(path = %path))]
    async fn get(
        &self,
        path: &str,
        range: Option<std::ops::Range<usize>>,
    ) -> Result<String, MemoryError> {
        let full_path = self.resolve_path(path);
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|e| MemoryError::GetFailed(format!("{}: {e}", full_path.display())))?;

        match range {
            Some(r) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = r.start.min(lines.len());
                let end = r.end.min(lines.len());
                Ok(lines[start..end].join("\n"))
            }
            None => Ok(content),
        }
    }
}

/// Serialize a vector into the compact `[f32]` form accepted by sqlite-vec.
fn format_vec(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// Cosine similarity between two vectors. Returns 0.0 if either is zero-length.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Multiplicative age-decay factor for episodic scores: `0.5^(age/half_life)`.
/// Fresh notes (age <= 0) keep their full score (1.0).
fn decay_factor(age_days: f32, half_life_days: f32) -> f32 {
    let half = half_life_days.max(0.001);
    0.5_f32.powf(age_days.max(0.0) / half)
}

/// Fuse ranked lists with reciprocal rank fusion.
fn reciprocal_rank_fusion(
    lists: Vec<Vec<impl AsRef<str>>>,
    top_k: usize,
    k: f32,
) -> Vec<(String, f32)> {
    let mut scores: HashMap<String, f32> = HashMap::new();

    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            let id = id.as_ref().to_string();
            let score = 1.0 / (rank as f32 + k);
            *scores.entry(id).or_default() += score;
        }
    }

    let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_k);
    ranked
}

fn sqlx_err_to_memory_err(err: sqlx::Error) -> MemoryError {
    MemoryError::SearchFailed(format!("database error: {err}"))
}

fn json_err_to_memory_err(err: serde_json::Error) -> MemoryError {
    MemoryError::SearchFailed(format!("serialization error: {err}"))
}

fn io_err_to_memory_err(err: std::io::Error) -> MemoryError {
    MemoryError::SearchFailed(format!("io error: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;

    #[tokio::test]
    async fn opens_database_in_wal_mode() {
        let collection = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let embedder = Arc::new(FakeEmbedder::new(16));
        let backend = SqliteVecBackend::open(collection.path(), workspace.path(), embedder)
            .await
            .unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&backend.db)
            .await
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn decay_factor_is_one_for_fresh_notes() {
        assert!((decay_factor(0.0, 30.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn decay_factor_halves_at_half_life() {
        let f = decay_factor(30.0, 30.0);
        assert!((f - 0.5).abs() < 1e-6, "got {f}");
    }

    #[test]
    fn decay_factor_quarters_at_two_half_lives() {
        let f = decay_factor(60.0, 30.0);
        assert!((f - 0.25).abs() < 1e-6, "got {f}");
    }

    #[test]
    fn cosine_of_identical_is_one() {
        let v = [1.0f32, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_orthogonal_is_zero() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert!(cosine(&a, &b).abs() < 1e-6);
    }
}
