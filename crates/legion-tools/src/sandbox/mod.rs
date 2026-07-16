use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod cube;
pub mod local;
pub mod policy;
pub mod restricted;

pub use cube::CubeSandboxBackend;
pub use local::LocalSandboxBackend;
pub use policy::pre_exec_guard;
pub use restricted::RestrictedSandboxBackend;

/// Result of executing a command inside a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecResult {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            exit_code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
}

/// Errors that can occur when interacting with a sandbox backend.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox backend is not configured")]
    NotConfigured,
    #[error("sandbox request failed: {0}")]
    RequestFailed(String),
    #[error("sandbox API error: {status} {message}")]
    ApiError { status: u16, message: String },
    #[error("sandbox command stream error: {0}")]
    StreamError(String),
    #[error("sandbox command timed out")]
    Timeout,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<reqwest::Error> for SandboxError {
    fn from(e: reqwest::Error) -> Self {
        SandboxError::RequestFailed(e.to_string())
    }
}

/// Capabilities advertised by a sandbox backend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxCapabilities {
    pub filesystem_isolation: bool,
    pub network_isolation: bool,
    pub process_isolation: bool,
    pub reusable: bool,
}

/// Sandbox execution profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// No isolation; execute directly on the host.
    Off,
    /// OS-native lightweight isolation (Linux namespaces / macOS sandbox-exec).
    #[default]
    Restricted,
    /// Remote Cube MicroVM.
    Cube,
}

impl std::str::FromStr for SandboxMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "restricted" => Ok(Self::Restricted),
            "cube" => Ok(Self::Cube),
            _ => Err(format!("unknown sandbox mode: {s}")),
        }
    }
}

/// Lifecycle scope of a sandbox instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxScope {
    /// One sandbox shared by the whole Gateway process.
    #[default]
    Shared,
    /// One sandbox per agent.
    PerAgent,
    /// One sandbox per session.
    PerSession,
}

/// Network policy for a restricted sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    /// No network access.
    #[default]
    None,
    /// Allow outbound access to the listed domains.
    Allowlist(Vec<String>),
}

/// Seccomp restriction level for Linux restricted sandboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeccompLevel {
    /// No seccomp filter.
    Off,
    /// Block a small set of high-risk syscalls (pivot_root, mount, etc.).
    #[default]
    Basic,
    /// Strict whitelist (not implemented yet).
    Strict,
}

/// Configuration for the restricted sandbox backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RestrictedConfig {
    /// Optional rootfs directory. When `None` the backend uses a temporary
    /// minimal rootfs or OS-native isolation (sandbox-exec / bwrap).
    pub rootfs: Option<PathBuf>,
    /// Paths that should be writable inside the sandbox. The workspace is
    /// always included implicitly.
    pub writable_paths: Vec<PathBuf>,
    /// Paths exposed read-only in addition to the system defaults.
    pub read_only_paths: Vec<PathBuf>,
    /// Network policy.
    pub network: NetworkPolicy,
    /// Environment variables allowed to pass through.
    pub env_whitelist: Vec<String>,
    /// Seccomp level (Linux only).
    pub seccomp: SeccompLevel,
}

/// Reason a sandbox profile is unavailable on the current platform.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum SandboxUnavailableReason {
    #[error(
        "Linux namespace sandbox requires a supported helper (bwrap/unshare) and user namespaces"
    )]
    LinuxNamespaceUnavailable,
    #[error("macOS sandbox-exec not found")]
    MacosSandboxExecMissing,
    #[error("Cube backend unreachable: {0}")]
    CubeUnreachable(String),
    #[error("platform {0} has no native restricted sandbox")]
    UnsupportedPlatform(String),
}

/// Check whether `mode` can be satisfied on the current platform.
pub fn sandbox_available(mode: SandboxMode) -> Result<(), SandboxUnavailableReason> {
    match mode {
        SandboxMode::Off => Ok(()),
        SandboxMode::Restricted => restricted::available(),
        SandboxMode::Cube => Ok(()), // Actual reachability is checked at runtime by the backend.
    }
}

/// Abstraction over command execution environments.
#[async_trait]
pub trait SandboxBackend: Send + Sync + std::fmt::Debug {
    /// Execute `command` with `cwd` as the working directory.
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
    ) -> Result<ExecResult, SandboxError>;

    /// Capabilities provided by this backend.
    fn capabilities(&self) -> SandboxCapabilities;
}

/// Configuration used to build a sandbox backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxBackendConfig {
    /// Backend identifier: "local" or "cube".
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Base URL of the CubeAPI service.
    #[serde(default = "default_cube_api_url")]
    pub api_url: String,
    /// CubeSandbox template ID.
    pub template_id: Option<String>,
    /// API key / access token for CubeAPI authentication.
    pub api_key: Option<String>,
    /// Sandbox domain used to construct envd hostnames (e.g. "cube.app").
    #[serde(default = "default_cube_domain")]
    pub domain: String,
    /// Optional proxy node IP for direct host:port access to sandbox services.
    pub proxy_node_ip: Option<String>,
    /// Optional proxy port when `proxy_node_ip` is set.
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    /// Optional override for the envd process API base URL. When set, it takes
    /// precedence over the hostname derived from the sandbox ID and domain.
    /// Useful for testing or when the envd endpoint is exposed directly.
    pub envd_override: Option<String>,
    /// Default idle timeout for newly created sandboxes in seconds.
    #[serde(default = "default_sandbox_timeout")]
    pub timeout_seconds: u64,
}

fn default_backend() -> String {
    "local".to_string()
}

fn default_cube_api_url() -> String {
    "http://127.0.0.1:3000".to_string()
}

fn default_cube_domain() -> String {
    "cube.app".to_string()
}

fn default_proxy_port() -> u16 {
    80
}

fn default_sandbox_timeout() -> u64 {
    300
}
