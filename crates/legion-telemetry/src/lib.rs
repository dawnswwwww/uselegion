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
