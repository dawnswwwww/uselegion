//! Small shared utilities used across the workspace: timestamps and id
//! generation. Kept dependency-free so every crate can rely on them.

/// Minimal ISO-like UTC timestamp (`<secs>.<millis>Z`) without adding a
/// chrono dependency.
pub fn iso_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}.{:03}Z", d.as_secs(), d.subsec_millis()))
        .unwrap_or_default()
}

/// Process-global monotonically increasing id counter, shared by the gateway,
/// channel, and host id formats (`gw-N`, `msg-N`, `run-N`).
pub fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Lock a `std::sync::Mutex`, recovering from poisoning instead of panicking.
///
/// A panic while another thread held the lock poisons the mutex; for the
/// in-memory state these mutexes guard the data is still usable, so we take
/// the guard out of the poison error rather than cascading one panic into
/// every subsequent `lock()` call.
pub fn lock_recover<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_now_has_seconds_millis_and_z_suffix() {
        let ts = iso_now();
        let stripped = ts.strip_suffix('Z').expect("timestamp ends with Z");
        let (secs, millis) = stripped.split_once('.').expect("timestamp has millis");
        assert!(secs.parse::<u64>().is_ok());
        assert_eq!(millis.len(), 3);
        assert!(millis.parse::<u64>().is_ok());
    }

    #[test]
    fn next_id_is_monotonic() {
        let a = next_id();
        let b = next_id();
        assert!(b > a);
    }

    #[test]
    fn lock_recover_survives_poisoning() {
        let mutex = std::sync::Mutex::new(1u32);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = lock_recover(&mutex);
            *guard = 2;
            panic!("poison the mutex");
        }));
        assert!(mutex.is_poisoned());
        // A poisoned mutex must still yield its guard instead of panicking.
        assert_eq!(*lock_recover(&mutex), 2);
    }
}
