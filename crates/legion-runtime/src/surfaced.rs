use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

/// Durable, per-session set of memory note ids already surfaced to the prompt
/// (Phase C). Lets the recall path suppress memories that were already injected
/// in an earlier turn of the same conversation, surviving process restarts.
///
/// Stored as one small JSON file per session under
/// `<base_dir>/agents/<agent_id>/surfaced/<hash(session_key)>.json`. Hashing the
/// session key keeps the filename free of `:` and other unsafe characters.
#[derive(Debug, Clone)]
pub struct SurfacedStore {
    base_dir: PathBuf,
}

impl Default for SurfacedStore {
    fn default() -> Self {
        Self {
            base_dir: legion_core::fs::legion_home(),
        }
    }
}

impl SurfacedStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    fn path(&self, agent_id: &str, session_key: &str) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        session_key.hash(&mut hasher);
        let name = format!("{:016x}.json", hasher.finish());
        self.base_dir
            .join("agents")
            .join(agent_id)
            .join("surfaced")
            .join(name)
    }

    /// Load the surfaced id set for a session. Missing/corrupt file → empty set.
    pub async fn load(&self, agent_id: &str, session_key: &str) -> HashSet<String> {
        let path = self.path(agent_id, session_key);
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => serde_json::from_str::<Vec<String>>(&text)
                .unwrap_or_default()
                .into_iter()
                .collect(),
            Err(_) => HashSet::new(),
        }
    }

    /// Union `new_ids` into the session's surfaced set and persist it.
    pub async fn append(&self, agent_id: &str, session_key: &str, new_ids: &[String]) {
        if new_ids.is_empty() {
            return;
        }
        let path = self.path(agent_id, session_key);
        let mut set = self.load(agent_id, session_key).await;
        let before = set.len();
        set.extend(new_ids.iter().cloned());
        if set.len() == before && new_ids.iter().all(|id| set.contains(id)) {
            return;
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!(error = %e, "surfaced store: failed to create dir");
                return;
            }
        }
        let mut ids: Vec<String> = set.into_iter().collect();
        ids.sort();
        match serde_json::to_string(&ids) {
            Ok(json) => self.atomic_write(&path, &json).await,
            Err(e) => tracing::warn!(error = %e, "surfaced store: serialize failed"),
        }
    }

    async fn atomic_write(&self, path: &Path, json: &str) {
        if let Err(e) = legion_core::fs::atomic_write_async(path, json.as_bytes()).await {
            tracing::warn!(error = %e, "surfaced store: write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (SurfacedStore, TempDir) {
        let dir = TempDir::new().unwrap();
        (SurfacedStore::new(dir.path()), dir)
    }

    #[tokio::test]
    async fn load_missing_returns_empty() {
        let (store, _dir) = store();
        let set = store
            .load("main", "agent:main:dm:webchat:default:direct:u1")
            .await;
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn append_then_load_round_trips() {
        let (store, _dir) = store();
        let key = "agent:main:dm:webchat:default:direct:u1";
        store.append("main", key, &["a".into(), "b".into()]).await;
        let set = store.load("main", key).await;
        assert!(set.contains("a"));
        assert!(set.contains("b"));
        assert_eq!(set.len(), 2);
    }

    #[tokio::test]
    async fn append_unions_across_calls() {
        let (store, _dir) = store();
        let key = "agent:main:dm:webchat:default:direct:u1";
        store.append("main", key, &["a".into()]).await;
        store.append("main", key, &["b".into()]).await;
        let set = store.load("main", key).await;
        assert_eq!(set.len(), 2);
    }

    #[tokio::test]
    async fn empty_append_is_noop() {
        let (store, _dir) = store();
        let key = "agent:main:dm:webchat:default:direct:u1";
        store.append("main", key, &[]).await;
        assert!(store.load("main", key).await.is_empty());
    }
}
