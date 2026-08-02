//! Escape-prevention policy for sandboxed command execution.
//!
//! These checks run before the sandbox backend and enforce deny-lists that are
//! independent of the OS-specific isolation mechanism (namespaces,
//! sandbox-exec, Cube).

use super::{RestrictedConfig, SandboxError};
use std::path::{Path, PathBuf};

/// Paths that must never be written by a sandboxed command, regardless of the
/// configured writable paths.
pub fn sensitive_paths() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let legion_home = legion_core::fs::legion_home();
    vec![
        legion_home.join("legion.json"),
        legion_home.join("agents"),
        home.join(".ssh"),
        home.join(".gnupg"),
        PathBuf::from("/etc"),
        PathBuf::from("/usr"),
    ]
}

/// Check whether `path` is inside any of the configured writable paths.
///
/// The workspace is always considered writable.
pub fn is_within_writable(path: &Path, workspace: &Path, cfg: &RestrictedConfig) -> bool {
    let path = resolve_absolute(path, workspace);
    let workspace = resolve_absolute(workspace, workspace);
    if path.starts_with(&workspace) {
        return true;
    }
    for writable in &cfg.writable_paths {
        let writable = resolve_absolute(writable, &workspace);
        if path.starts_with(&writable) {
            return true;
        }
    }
    false
}

fn resolve_absolute(path: &Path, workspace: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

/// Detect a git bare-repository escape attempt in `cwd`.
///
/// A malicious workspace can plant a fake `.git` directory with a
/// `core.fsmonitor` hook that is executed by the host git binary when the
/// sandboxed command runs git. Removing known bare-repo marker files before
/// execution mitigates this.
pub fn scrub_bare_git_repo(cwd: &Path) -> Result<(), SandboxError> {
    let git_dir = cwd.join(".git");
    if !git_dir.is_dir() {
        return Ok(());
    }

    // Marker files that, when present in a directory that is not a real git
    // worktree, can trick git into treating it as a bare repo.
    let markers = ["HEAD", "config", "objects", "refs"];
    for marker in &markers {
        let path = git_dir.join(marker);
        if path.exists() {
            tracing::warn!(
                path = %path.display(),
                "removing suspicious bare-repo marker before sandboxed exec"
            );
            if path.is_dir() {
                std::fs::remove_dir_all(&path).map_err(SandboxError::Io)?;
            } else {
                std::fs::remove_file(&path).map_err(SandboxError::Io)?;
            }
        }
    }
    Ok(())
}

/// Pre-execution guard: deny obvious escape attempts before the backend runs.
pub fn pre_exec_guard(
    command: &str,
    cwd: &Path,
    workspace: &Path,
    cfg: &RestrictedConfig,
) -> Result<(), SandboxError> {
    scrub_bare_git_repo(cwd)?;

    // Reject commands that appear to write to sensitive paths.
    for sensitive in sensitive_paths() {
        if command_contains_path_write(command, &sensitive) {
            return Err(SandboxError::RequestFailed(format!(
                "command would write to sensitive path: {}",
                sensitive.display()
            )));
        }
    }

    // If the command mentions an absolute path outside the writable set,
    // deny any write-like redirection toward it. This is a coarse
    // first-line check; the backend isolation is the real enforcement.
    for token in command.split_whitespace() {
        if let Some(path_str) = token.strip_prefix(">>") {
            check_writable_path(Path::new(path_str), workspace, cfg)?;
        } else if let Some(path_str) = token.strip_prefix(">") {
            check_writable_path(Path::new(path_str), workspace, cfg)?;
        }
    }

    Ok(())
}

fn check_writable_path(
    path: &Path,
    workspace: &Path,
    cfg: &RestrictedConfig,
) -> Result<(), SandboxError> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };

    if !is_within_writable(&resolved, workspace, cfg) {
        return Err(SandboxError::RequestFailed(format!(
            "write to {} is outside the sandbox writable area",
            resolved.display()
        )));
    }
    Ok(())
}

fn command_contains_path_write(command: &str, sensitive: &Path) -> bool {
    let needle = sensitive.to_string_lossy();
    // Very coarse: a redirection into the sensitive path.
    command.contains(&format!("> {}", needle)) || command.contains(&format!(">> {}", needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sensitive_paths_include_legion_config() {
        let sensitive: Vec<_> = sensitive_paths()
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(sensitive.iter().any(|p| p.contains(".ssh")));
        assert!(sensitive.iter().any(|p| p.contains(".gnupg")));
        assert!(sensitive.iter().any(|p| p.contains(".legion")));
    }

    #[test]
    fn writable_paths_include_workspace() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let cfg = RestrictedConfig::default();

        assert!(is_within_writable(&workspace, &workspace, &cfg));
        assert!(is_within_writable(
            &workspace.join("file.txt"),
            &workspace,
            &cfg
        ));
    }

    #[test]
    fn writable_paths_include_configured_paths() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        let extra = tmp.path().join("extra");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&extra).unwrap();

        let cfg = RestrictedConfig {
            writable_paths: vec![extra.clone()],
            ..Default::default()
        };

        assert!(is_within_writable(&extra.join("x"), &workspace, &cfg));
        assert!(!is_within_writable(
            &tmp.path().join("other"),
            &workspace,
            &cfg
        ));
    }

    #[test]
    fn scrub_bare_git_repo_removes_markers() {
        let tmp = TempDir::new().unwrap();
        let git = tmp.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(git.join("objects")).unwrap();

        scrub_bare_git_repo(tmp.path()).unwrap();

        assert!(!git.join("HEAD").exists());
        assert!(!git.join("objects").exists());
    }

    #[test]
    fn pre_exec_guard_blocks_write_outside_workspace() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let cfg = RestrictedConfig::default();

        let result = pre_exec_guard("echo hi > /etc/passwd", &workspace, &workspace, &cfg);
        assert!(result.is_err());
    }

    #[test]
    fn pre_exec_guard_allows_write_inside_workspace() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let cfg = RestrictedConfig::default();

        let result = pre_exec_guard("echo hi > file.txt", &workspace, &workspace, &cfg);
        assert!(result.is_ok());
    }

    #[test]
    fn pre_exec_guard_blocks_append_outside_workspace() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let cfg = RestrictedConfig::default();

        let outside = tmp.path().join("outside.txt");
        let command = format!("echo hi >>{}", outside.display());
        let err = pre_exec_guard(&command, &workspace, &workspace, &cfg).unwrap_err();
        assert!(
            err.to_string()
                .contains("outside the sandbox writable area"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pre_exec_guard_allows_append_inside_workspace() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let cfg = RestrictedConfig::default();

        let command = format!("echo hi >>{}/log.txt", workspace.display());
        let result = pre_exec_guard(&command, &workspace, &workspace, &cfg);
        assert!(result.is_ok());
    }

    #[test]
    fn pre_exec_guard_blocks_write_to_ssh_dir() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let cfg = RestrictedConfig::default();

        let ssh_dir = sensitive_paths()
            .into_iter()
            .find(|p| p.ends_with(".ssh"))
            .expect(".ssh must be a sensitive path");
        let target = ssh_dir.join("authorized_keys");

        for command in [
            format!("echo key > {}", target.display()),
            format!("echo key >> {}", target.display()),
        ] {
            let err = pre_exec_guard(&command, &workspace, &workspace, &cfg).unwrap_err();
            assert!(
                err.to_string().contains("sensitive path"),
                "command {command:?} produced unexpected error: {err}"
            );
        }
    }
}
