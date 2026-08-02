//! Shared job id generation for cron jobs and scheduler tools.
//!
//! All automation job ids are produced by a single process-level counter so
//! that different creation paths (gateway RPC, scheduler tools, webhooks)
//! cannot collide and silently overwrite one another.

use chrono::Utc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Generate a unique job id with an optional prefix.
///
/// The returned id has the shape `<prefix>-<nanos>-<counter>`. The counter is
/// shared across the whole process, so ids are monotonic and unique as long as
/// the nanosecond timestamp does not move backwards.
pub fn generate_job_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}-{}",
        prefix,
        Utc::now().timestamp_nanos_opt().unwrap_or(0),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_job_id_includes_prefix_and_is_unique() {
        let a = generate_job_id("cron");
        let b = generate_job_id("cron");
        assert_ne!(a, b);
        assert!(a.starts_with("cron-"));
    }
}
