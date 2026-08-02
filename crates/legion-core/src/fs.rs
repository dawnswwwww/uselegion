//! Shared filesystem and path helpers used across the workspace.

use std::path::{Path, PathBuf};

/// Expand a leading `~/` to the user's home directory, returning the path
/// unchanged when it has no tilde prefix or the home directory is unknown.
///
/// Home resolution goes through [`dirs::home_dir`] (which prefers `$HOME` on
/// Unix but also handles other platforms) rather than reading the `HOME`
/// environment variable directly, so all callers agree on the semantics.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// The Legion home directory (`~/.legion`), falling back to a relative
/// `.legion` when the home directory cannot be determined.
pub fn legion_home() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".legion"))
        .unwrap_or_else(|| PathBuf::from(".legion"))
}

/// Crash-safe write: write `bytes` into a uniquely-named temp file in the same
/// directory, then rename over the target so a crash mid-write never leaves a
/// truncated file behind. On failure the temp file is removed on a
/// best-effort basis.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = tmp_path_for(path);
    if let Err(err) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Async (tokio) variant of [`atomic_write`].
pub async fn atomic_write_async(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = tmp_path_for(path);
    if let Err(err) = tokio::fs::write(&tmp, bytes).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }
    if let Err(err) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }
    Ok(())
}

/// Unique temp path next to `path` for atomic write-then-rename persistence.
fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!("{file_name}.tmp-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/projects"), home.join("projects"));
    }

    #[test]
    fn expand_tilde_leaves_other_paths_alone() {
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
        // A bare `~` (no slash) is not expanded.
        assert_eq!(expand_tilde("~"), PathBuf::from("~"));
    }

    #[test]
    fn legion_home_is_under_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(legion_home(), home.join(".legion"));
    }

    #[test]
    fn atomic_write_round_trips_and_leaves_no_residue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let residue: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(residue.is_empty());
    }

    #[tokio::test]
    async fn atomic_write_async_round_trips_and_leaves_no_residue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        atomic_write_async(&path, b"hello").await.unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"hello");
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut residue = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".tmp-") {
                residue.push(name);
            }
        }
        assert!(residue.is_empty());
    }
}
