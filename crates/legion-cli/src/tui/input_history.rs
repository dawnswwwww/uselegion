//! Persistent, per-workspace input history.
//!
//! The TUI's ↑/↓ recall (and the `Ctrl+R` search popup) read from the input
//! history. Historically this lived only in memory and reset on every launch,
//! so a fresh session could never recall inputs typed in previous sessions.
//!
//! [`InputHistoryStore`] is the single source of truth for that history. When
//! constructed for a workspace it transparently persists to a JSON file keyed
//! by the workspace's canonical path, so all sessions launched in the same
//! project share one history. The default constructor ([`InputHistoryStore`]'s
//! `Default` impl) keeps the old behaviour — an in-memory list that is never
//! written to disk — which tests and `AppState::default()` rely on.
//!
//! Persisted entries are already paste-expanded before they ever reach the
//! store (see `events.rs`): the composer's `[...Pasted text #N ...]`
//! placeholders are session-local ids with no meaning across sessions, so only
//! expanded text is durable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Maximum number of entries kept on disk. Older entries are dropped when a
/// new one is recorded past this bound.
const CAP: usize = 2000;

/// On-disk layout for the history file.
#[derive(Serialize, Deserialize)]
struct HistoryFile {
    /// The canonical workspace path this file belongs to, kept purely for
    /// human inspection of the JSON on disk.
    workspace: String,
    entries: Vec<String>,
}

/// Single source of truth for the TUI input history.
///
/// `path == None` means memory-only (the legacy behaviour). Otherwise every
/// [`InputHistoryStore::record`] also atomically rewrites the JSON file so the
/// history survives across sessions.
#[derive(Clone, Default)]
pub(crate) struct InputHistoryStore {
    path: Option<PathBuf>,
    /// The canonical workspace path, written into the JSON for human
    /// inspection. `None` for memory-only stores.
    workspace: Option<String>,
    entries: Vec<String>,
}

impl InputHistoryStore {
    /// Build an in-memory history pre-seeded with `entries` (newest last),
    /// for tests that need a deterministic history without going through
    /// `record` (which dedupes and trims).
    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<String>) -> Self {
        Self {
            path: None,
            workspace: None,
            entries,
        }
    }

    /// Load the history from `path`, starting empty if the file does not exist
    /// yet or cannot be read/parsed. The store becomes persistent: subsequent
    /// [`Self::record`] calls rewrite `path`. `workspace` is the human-readable
    /// canonical workspace path, embedded into the JSON for inspection.
    pub(crate) fn load(path: PathBuf, workspace: String) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<HistoryFile>(&text) {
                Ok(file) => file.entries,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "input history file is unreadable; starting empty",
                    );
                    Vec::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to read input history file; starting empty",
                );
                Vec::new()
            }
        };
        Self {
            path: Some(path),
            workspace: Some(workspace),
            entries,
        }
    }

    /// All recorded entries, newest last.
    pub(crate) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record a submitted input. A duplicate of the most recent entry is
    /// ignored (so re-sending the same line does not flood the history), the
    /// list is trimmed to [`CAP`], and — when persistent — the file is
    /// rewritten atomically.
    pub(crate) fn record(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.entries.last().is_some_and(|last| last == line) {
            return;
        }
        self.entries.push(line.to_string());
        if self.entries.len() > CAP {
            let drop = self.entries.len() - CAP;
            self.entries.drain(0..drop);
        }
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let workspace = self.workspace.clone().unwrap_or_default();
        if let Err(err) = write_history(path, &workspace, &self.entries) {
            tracing::warn!(path = %path.display(), error = %err, "failed to persist input history");
        }
    }
}

/// Serialize `entries` to `path` via an atomic write-then-rename. `workspace`
/// is stored verbatim purely for human inspection of the JSON on disk.
fn write_history(path: &Path, workspace: &str, entries: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = HistoryFile {
        workspace: workspace.to_string(),
        entries: entries.to_vec(),
    };
    let text = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
    legion_core::fs::atomic_write(path, text.as_bytes())
}

/// Compute the stable file path for a workspace's input history, together
/// with the canonical workspace path string (for embedding into the JSON).
///
/// The path is `~/.legion/history/<hhhhhhhh>.json` where the segment is the
/// FNV-1a 64-bit hash of the canonical path, hex-encoded. FNV-1a is used
/// (rather than [`std::collections::hash_map::DefaultHasher`]) because its
/// result is stable across Rust versions and process restarts, which is
/// required for the path to name the same file across sessions.
pub(crate) fn workspace_history_path(workspace: &Path) -> (PathBuf, String) {
    let canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let canon_str = canon.to_string_lossy().into_owned();
    let hash = fnv1a_64(canon_str.as_bytes());
    let path = legion_core::fs::legion_home()
        .join("history")
        .join(format!("{hash:016x}.json"));
    (path, canon_str)
}

/// FNV-1a 64-bit hash. Deterministic across process restarts and Rust versions.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path() -> PathBuf {
        tempfile::NamedTempFile::new()
            .unwrap()
            .into_temp_path()
            .keep()
            .unwrap()
    }

    /// Build a persistent store at `path` for tests. The workspace label is
    /// irrelevant to persistence behaviour; a fixed marker keeps tests simple.
    fn load_at(path: PathBuf) -> InputHistoryStore {
        InputHistoryStore::load(path, "<test>".to_string())
    }

    #[test]
    fn default_is_empty_and_never_persists() {
        let mut store = InputHistoryStore::default();
        store.record("hello");
        store.record("world");
        assert_eq!(store.entries(), &["hello", "world"]);
        // No path -> nothing to read back; a fresh store is still empty.
        let again = InputHistoryStore::default();
        assert!(again.entries().is_empty());
    }

    #[test]
    fn load_missing_file_is_empty() {
        let path = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("does_not_exist.json");
        let store = load_at(path);
        assert!(store.entries().is_empty());
    }

    #[test]
    fn record_round_trips_through_disk() {
        let path = tmp_path();
        // Start empty (file may or may not exist yet from keep()).
        std::fs::remove_file(&path).ok();
        let mut store = load_at(path.clone());
        assert!(store.entries().is_empty());

        store.record("first");
        store.record("second");

        let reloaded = load_at(path);
        assert_eq!(reloaded.entries(), &["first", "second"]);
    }

    #[test]
    fn record_dedupes_consecutive_duplicate() {
        let path = tempfile::tempdir().unwrap().keep().join("h.json");
        let mut store = load_at(path);
        store.record("same");
        store.record("same");
        store.record("same");
        assert_eq!(store.entries(), &["same"]);
    }

    #[test]
    fn record_dedupes_only_consecutive() {
        // Non-consecutive repeats are kept — they reflect distinct turns.
        let path = tempfile::tempdir().unwrap().keep().join("h.json");
        let mut store = load_at(path);
        store.record("a");
        store.record("b");
        store.record("a");
        assert_eq!(store.entries(), &["a", "b", "a"]);
    }

    #[test]
    fn record_ignores_empty_and_whitespace() {
        let path = tempfile::tempdir().unwrap().keep().join("h.json");
        let mut store = load_at(path);
        store.record("   ");
        store.record("");
        store.record("\t\n");
        assert!(store.entries().is_empty());
    }

    #[test]
    fn record_trims() {
        let path = tempfile::tempdir().unwrap().keep().join("h.json");
        let mut store = load_at(path);
        store.record("  padded  ");
        assert_eq!(store.entries(), &["padded"]);
    }

    #[test]
    fn cap_drops_oldest() {
        let path = tempfile::tempdir().unwrap().keep().join("h.json");
        let mut store = load_at(path);
        for i in 0..(CAP + 50) {
            store.record(&format!("line-{i}"));
        }
        assert_eq!(store.entries().len(), CAP);
        // Oldest were dropped, newest retained.
        assert_eq!(store.entries().first(), Some(&"line-50".to_string()));
        assert_eq!(store.entries().last(), Some(&format!("line-{}", CAP + 49)));
    }

    #[test]
    fn corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("h.json");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let store = load_at(path);
        assert!(store.entries().is_empty());
    }

    #[test]
    fn same_workspace_path_is_stable_across_constructors() {
        let ws = Path::new("/tmp/legion-project-x");
        let (p1, _) = workspace_history_path(ws);
        let (p2, _) = workspace_history_path(ws);
        assert_eq!(p1, p2);
        assert!(p1.parent().is_some_and(|p| p.ends_with("history")));
        assert!(p1.to_string_lossy().ends_with(".json"));
    }

    #[test]
    fn different_workspaces_get_different_files() {
        let (a, _) = workspace_history_path(Path::new("/tmp/proj-a"));
        let (b, _) = workspace_history_path(Path::new("/tmp/proj-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn fnv_is_deterministic() {
        let h1 = fnv1a_64(b"/Users/me/proj");
        let h2 = fnv1a_64(b"/Users/me/proj");
        assert_eq!(h1, h2);
        assert_ne!(h1, fnv1a_64(b"/Users/me/other"));
    }
}
