use std::path::{Path, PathBuf};

pub mod client;
pub mod session_metrics;
pub mod unified_log;

pub use client::TelemetryClient;
pub use session_metrics::SessionMetric;
pub use unified_log::{LogEntry, LogLevel, LogSource, UnifiedLog};

/// Errors that can occur while operating the telemetry subsystem.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Expand a leading `~` to the user's home directory, falling back to the
/// literal path when `HOME` is unavailable.
pub(crate) fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix("~/")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}
