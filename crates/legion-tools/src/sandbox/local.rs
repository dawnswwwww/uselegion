use async_trait::async_trait;
use std::path::Path;
use tracing::debug;

use super::{ExecResult, SandboxBackend, SandboxCapabilities, SandboxError};

/// Execute commands directly on the host machine.
#[derive(Debug, Default, Clone)]
pub struct LocalSandboxBackend;

impl LocalSandboxBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxBackend for LocalSandboxBackend {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
    ) -> Result<ExecResult, SandboxError> {
        debug!(command, ?cwd, timeout_secs, "executing local command");

        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(command);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command);
            c
        };

        cmd.current_dir(cwd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let output =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output())
                .await
                .map_err(|_| SandboxError::Timeout)?
                .map_err(SandboxError::Io)?;

        Ok(ExecResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem_isolation: false,
            network_isolation: false,
            process_isolation: false,
            reusable: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_backend_captures_stdout_stderr_and_exit_code() {
        let backend = LocalSandboxBackend::new();
        let result = backend
            .exec("echo hello && echo err >&2 && exit 42", Path::new("/"), 10)
            .await
            .unwrap();

        assert!(result.stdout.contains("hello"));
        assert!(result.stderr.contains("err"));
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn local_backend_respects_cwd() {
        let backend = LocalSandboxBackend::new();
        let result = backend.exec("pwd", Path::new("/tmp"), 10).await.unwrap();

        assert!(
            result.stdout.trim().ends_with("tmp"),
            "got: {}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn local_backend_times_out() {
        let backend = LocalSandboxBackend::new();
        let result = backend.exec("sleep 10", Path::new("/"), 1).await;
        assert!(matches!(result, Err(SandboxError::Timeout)));
    }
}
