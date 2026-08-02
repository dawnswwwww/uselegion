//! MCP protocol version negotiation support.
//!
//! Legion speaks every protocol revision from `2024-11-05` through the
//! stateless `2026-07-28` core. All revisions use the `YYYY-MM-DD` format, so
//! lexicographic string comparison orders them chronologically; the helpers
//! below compare deliberately against named milestones instead of scattering
//! bare string comparisons through the client code.

use serde_json::Value;
use std::sync::{Mutex, MutexGuard};

/// Protocol versions legion can negotiate, ordered newest to oldest. The
/// `initialize` fallback chain tries them in this order.
pub const SUPPORTED_VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// The newest protocol version legion supports.
pub const LATEST_VERSION: &str = SUPPORTED_VERSIONS[0];

/// First revision with the stateless protocol core: `initialize` /
/// `notifications/initialized` and the `Mcp-Session-Id` header are retired,
/// and every request is self-describing (version, client identity and client
/// capabilities travel in `_meta`; streamable HTTP requests also carry
/// `Mcp-Method` / `Mcp-Name` routing headers).
pub const STATELESS_VERSION: &str = "2026-07-28";

/// First revision whose streamable HTTP transport requires the
/// `MCP-Protocol-Version` header on post-initialize requests.
pub const STREAMABLE_HTTP_VERSION: &str = "2025-03-26";

/// Whether `version` uses the stateless protocol core (2026-07-28 or newer).
pub fn is_stateless(version: &str) -> bool {
    version >= STATELESS_VERSION
}

/// Whether streamable HTTP requests for `version` must carry the
/// `MCP-Protocol-Version` header (2025-03-26 or newer).
pub fn requires_version_header(version: &str) -> bool {
    version >= STREAMABLE_HTTP_VERSION
}

/// Per-client protocol state: the negotiated (or in-progress) version, the
/// capabilities the server reported during negotiation, and the optional
/// config pin.
///
/// Uses a `std::sync::Mutex` because the critical sections are tiny value
/// swaps; no `.await` is ever held across a lock.
pub struct ProtocolState {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    version: String,
    capabilities: Value,
    pinned: Option<String>,
}

impl ProtocolState {
    /// Start at the pinned version when configured, otherwise at the latest
    /// supported version (the first fallback-chain candidate).
    pub fn new(pinned: Option<String>) -> Self {
        let version = pinned.clone().unwrap_or_else(|| LATEST_VERSION.to_string());
        Self {
            inner: Mutex::new(Inner {
                version,
                capabilities: Value::Object(serde_json::Map::new()),
                pinned,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Current protocol version: the negotiated one once `initialize` (or
    /// `server/discover`) succeeded, or the fallback candidate being tried.
    pub fn version(&self) -> String {
        self.lock().version.clone()
    }

    /// Capabilities reported by the server (`{}` before negotiation).
    pub fn capabilities(&self) -> Value {
        self.lock().capabilities.clone()
    }

    /// Config-pinned protocol version, if any.
    pub fn pinned(&self) -> Option<String> {
        self.lock().pinned.clone()
    }

    /// Record a negotiation outcome (or the fallback attempt in progress).
    pub fn set_negotiated(&self, version: String, capabilities: Value) {
        let mut guard = self.lock();
        guard.version = version;
        guard.capabilities = capabilities;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_versions_are_newest_first() {
        assert_eq!(SUPPORTED_VERSIONS[0], LATEST_VERSION);
        assert_eq!(LATEST_VERSION, STATELESS_VERSION);
        let mut sorted = SUPPORTED_VERSIONS.to_vec();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sorted, SUPPORTED_VERSIONS);
    }

    #[test]
    fn stateless_boundary_is_2026_07_28() {
        assert!(is_stateless("2026-07-28"));
        assert!(is_stateless("2027-01-01"));
        assert!(!is_stateless("2025-11-25"));
        assert!(!is_stateless("2024-11-05"));
    }

    #[test]
    fn version_header_required_from_2025_03_26() {
        assert!(requires_version_header("2026-07-28"));
        assert!(requires_version_header("2025-03-26"));
        assert!(!requires_version_header("2024-11-05"));
        // Older than our minimum, adopted permissively.
        assert!(!requires_version_header("2023-01-01"));
    }

    #[test]
    fn protocol_state_starts_at_latest_or_pin() {
        let state = ProtocolState::new(None);
        assert_eq!(state.version(), LATEST_VERSION);
        assert_eq!(state.capabilities(), serde_json::json!({}));
        assert_eq!(state.pinned(), None);

        let state = ProtocolState::new(Some("2025-06-18".to_string()));
        assert_eq!(state.version(), "2025-06-18");
        assert_eq!(state.pinned().as_deref(), Some("2025-06-18"));

        state.set_negotiated("2024-11-05".to_string(), serde_json::json!({"tools": {}}));
        assert_eq!(state.version(), "2024-11-05");
        assert_eq!(state.capabilities(), serde_json::json!({"tools": {}}));
    }
}
