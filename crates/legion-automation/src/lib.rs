//! Legion automation subsystem.
//!
//! Provides cron scheduling, periodic heartbeat turns, and a minimal task
//! ledger.

pub mod commitments;
pub mod cron;
pub mod flow;
pub mod heartbeat;
pub mod job_id;
pub mod task_runner;
pub mod tasks;

use std::path::PathBuf;

pub use commitments::LlmCommitmentExtractor;
pub use job_id::generate_job_id;

/// Return the default Legion workspace path (`~/.legion/workspace`).
pub fn home_workspace() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".legion").join("workspace"))
        .unwrap_or_else(|| PathBuf::from(".legion/workspace"))
}

/// Return the default Legion data directory (`~/.legion`).
pub fn home_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".legion"))
        .unwrap_or_else(|| PathBuf::from(".legion"))
}
