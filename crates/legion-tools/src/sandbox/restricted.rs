//! OS-native restricted sandbox backend.
//!
//! - Linux: uses `bwrap` (bubblewrap) when available, otherwise `unshare`.
//! - macOS: uses the system `sandbox-exec` binary.
//! - Other platforms: reports unavailable.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::debug;

use super::{
    ExecResult, NetworkPolicy, RestrictedConfig, SandboxBackend, SandboxCapabilities, SandboxError,
    SandboxUnavailableReason, pre_exec_guard,
};

/// A restricted sandbox backend backed by OS-native primitives.
#[derive(Debug, Clone)]
pub struct RestrictedSandboxBackend {
    cfg: RestrictedConfig,
}

impl RestrictedSandboxBackend {
    pub fn new(cfg: RestrictedConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl SandboxBackend for RestrictedSandboxBackend {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
    ) -> Result<ExecResult, SandboxError> {
        pre_exec_guard(command, cwd, cwd, &self.cfg)?;
        run_restricted(command, cwd, &self.cfg, timeout_secs).await
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem_isolation: true,
            network_isolation: self.cfg.network == NetworkPolicy::None,
            process_isolation: true,
            reusable: false,
        }
    }
}

/// Check whether the restricted backend is available on this platform.
pub fn available() -> Result<(), SandboxUnavailableReason> {
    platform_available()
}

// ---------------------------------------------------------------------------
// Platform-specific implementations
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn platform_available() -> Result<(), SandboxUnavailableReason> {
    if helper_binary("bwrap").is_some() || helper_binary("unshare").is_some() {
        Ok(())
    } else {
        Err(SandboxUnavailableReason::LinuxNamespaceUnavailable)
    }
}

#[cfg(target_os = "macos")]
fn platform_available() -> Result<(), SandboxUnavailableReason> {
    if helper_binary("sandbox-exec").is_some() {
        Ok(())
    } else {
        Err(SandboxUnavailableReason::MacosSandboxExecMissing)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_available() -> Result<(), SandboxUnavailableReason> {
    Err(SandboxUnavailableReason::UnsupportedPlatform(
        std::env::consts::OS.to_string(),
    ))
}

async fn run_restricted(
    command: &str,
    cwd: &Path,
    cfg: &RestrictedConfig,
    timeout_secs: u64,
) -> Result<ExecResult, SandboxError> {
    #[cfg(target_os = "linux")]
    {
        linux_run(command, cwd, cfg, timeout_secs).await
    }
    #[cfg(target_os = "macos")]
    {
        macos_run(command, cwd, cfg, timeout_secs).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(SandboxError::RequestFailed(format!(
            "restricted sandbox is not supported on {}",
            std::env::consts::OS
        )))
    }
}

// ---------------------------------------------------------------------------
// Linux (bubblewrap)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
async fn linux_run(
    command: &str,
    cwd: &Path,
    cfg: &RestrictedConfig,
    timeout_secs: u64,
) -> Result<ExecResult, SandboxError> {
    let bwrap = helper_binary("bwrap").ok_or_else(|| {
        SandboxError::RequestFailed(
            "restricted sandbox requires bwrap (bubblewrap) on Linux".to_string(),
        )
    })?;

    let workspace = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    let mut cmd = tokio::process::Command::new(&bwrap);
    cmd.arg("--unshare-all")
        .arg("--die-with-parent")
        .arg("--new-session")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--ro-bind")
        .arg("/")
        .arg("/");

    // Make the workspace writable at its original absolute path.
    cmd.arg("--bind").arg(&workspace).arg(&workspace);

    for writable in &cfg.writable_paths {
        let path = writable.canonicalize().unwrap_or_else(|_| writable.clone());
        cmd.arg("--bind").arg(&path).arg(&path);
    }

    for readonly in &cfg.read_only_paths {
        let path = readonly.canonicalize().unwrap_or_else(|_| readonly.clone());
        cmd.arg("--ro-bind").arg(&path).arg(&path);
    }

    // Network policy.
    if cfg.network == NetworkPolicy::None {
        cmd.arg("--unshare-net");
    }

    // Environment.
    cmd.arg("--clearenv");
    for key in &cfg.env_whitelist {
        if let Ok(value) = std::env::var(key) {
            cmd.arg("--setenv").arg(key).arg(value);
        }
    }

    cmd.arg("--chdir")
        .arg(&workspace)
        .arg("sh")
        .arg("-c")
        .arg(command);

    debug!(?cmd, "running restricted command in bwrap");
    run_command(&mut cmd, timeout_secs).await
}

// ---------------------------------------------------------------------------
// macOS (sandbox-exec)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
async fn macos_run(
    command: &str,
    cwd: &Path,
    cfg: &RestrictedConfig,
    timeout_secs: u64,
) -> Result<ExecResult, SandboxError> {
    let sandbox_exec = helper_binary("sandbox-exec").ok_or_else(|| {
        SandboxError::RequestFailed("sandbox-exec not found on macOS".to_string())
    })?;

    let workspace = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let profile = build_macos_profile(&workspace, cfg);

    let mut cmd = tokio::process::Command::new(&sandbox_exec);
    cmd.arg("-p").arg(&profile);
    cmd.arg("/bin/sh").arg("-c").arg(format!(
        "cd '{}' && {}",
        shell_escape(&workspace.to_string_lossy()),
        command
    ));

    debug!(?cmd, "running restricted command in sandbox-exec");
    run_command(&mut cmd, timeout_secs).await
}

#[cfg(target_os = "macos")]
fn build_macos_profile(workspace: &Path, cfg: &RestrictedConfig) -> String {
    let workspace_str = workspace.to_string_lossy();
    let mut lines = vec![
        "(version 1)".to_string(),
        "(deny default)".to_string(),
        "(allow file-read*)".to_string(),
        "(allow process*)".to_string(),
        "(allow job-creation)".to_string(),
    ];

    // Writable paths.
    lines.push(format!(
        "(allow file-write* (subpath \"{}\"))",
        workspace_str
    ));
    for writable in &cfg.writable_paths {
        let path = writable.canonicalize().unwrap_or_else(|_| writable.clone());
        lines.push(format!(
            "(allow file-write* (subpath \"{}\"))",
            path.display()
        ));
    }

    // Read-only paths.
    for readonly in &cfg.read_only_paths {
        let path = readonly.canonicalize().unwrap_or_else(|_| readonly.clone());
        lines.push(format!(
            "(allow file-read* (subpath \"{}\"))",
            path.display()
        ));
    }

    // Network.
    match &cfg.network {
        NetworkPolicy::None => {
            lines.push("(deny network*)".to_string());
            lines.push("(allow network-inbound (local ip \"localhost:*\"))".to_string());
        }
        NetworkPolicy::Allowlist(domains) => {
            for domain in domains {
                lines.push(format!(
                    "(allow network-outbound (remote ip \"{}:*\"))",
                    domain
                ));
            }
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async fn run_command(
    cmd: &mut tokio::process::Command,
    timeout_secs: u64,
) -> Result<ExecResult, SandboxError> {
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| SandboxError::Timeout)?
        .map_err(SandboxError::Io)?;

    Ok(ExecResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn helper_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|full| full.is_file())
    })
}

#[cfg(target_os = "macos")]
fn shell_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_show_isolation() {
        let backend = RestrictedSandboxBackend::new(RestrictedConfig::default());
        let caps = backend.capabilities();
        assert!(caps.filesystem_isolation);
        assert!(caps.process_isolation);
    }

    #[test]
    fn available_returns_result() {
        // Result depends on the host platform and installed helpers; assert it
        // matches the same capability detection the implementation uses.
        let result = available();

        #[cfg(target_os = "macos")]
        {
            if helper_binary("sandbox-exec").is_some() {
                assert!(
                    result.is_ok(),
                    "sandbox-exec is on PATH; restricted backend must be available"
                );
            } else {
                assert_eq!(
                    result,
                    Err(SandboxUnavailableReason::MacosSandboxExecMissing)
                );
            }
        }

        #[cfg(target_os = "linux")]
        {
            if helper_binary("bwrap").is_some() || helper_binary("unshare").is_some() {
                assert!(
                    result.is_ok(),
                    "bwrap/unshare is on PATH; restricted backend must be available"
                );
            } else {
                assert_eq!(
                    result,
                    Err(SandboxUnavailableReason::LinuxNamespaceUnavailable)
                );
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            assert!(matches!(
                result,
                Err(SandboxUnavailableReason::UnsupportedPlatform(_))
            ));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_contains_workspace_and_blocks_network() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let cfg = RestrictedConfig::default();
        let profile = build_macos_profile(&workspace, &cfg);

        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            workspace.display()
        )));
    }
}
