//! Protocol compatibility negotiation between Legion CLI and Gateway.
//!
//! This module defines a machine-readable compatibility range so the CLI and
//! Gateway can decide whether they can interoperate without relying on crate
//! version string equality.

use serde::{Deserialize, Serialize};

/// Current protocol revision used by this build.
pub const CURRENT_PROTOCOL_REVISION: u32 = 1;

/// Default minimum peer revision this build accepts.
pub const DEFAULT_MIN_PEER_REVISION: u32 = 1;

/// Default maximum peer revision this build accepts.
pub const DEFAULT_MAX_PEER_REVISION: u32 = 1;

/// Capability strings for optional RPC methods / events.
pub mod capabilities {
    pub const AGENT_RUN_V1: &str = "agent.run.v1";
    pub const SESSIONS_HISTORY_V1: &str = "sessions.history.v1";
    pub const APPROVAL_RESOLVE_V1: &str = "approval.resolve.v1";
    pub const FLOWS_RUN_V1: &str = "flows.run.v1";
    pub const NODES_INVOKE_V1: &str = "nodes.invoke.v1";
}

/// Compatibility information exchanged after authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolCompatibility {
    /// Protocol revision implemented by this peer.
    pub protocol_revision: u32,
    /// Minimum peer revision this peer can talk to.
    pub min_peer_revision: u32,
    /// Maximum peer revision this peer can talk to.
    pub max_peer_revision: u32,
    /// Human-readable product version (crate version in dev builds).
    pub product_version: String,
    /// Release identifier from the signed manifest (falls back to product
    /// version in dev builds that are not part of a release train).
    pub release_id: String,
    /// Optional capabilities advertised by this peer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

impl ProtocolCompatibility {
    /// Build a compatibility value for this crate.
    pub fn current() -> Self {
        Self {
            protocol_revision: CURRENT_PROTOCOL_REVISION,
            min_peer_revision: DEFAULT_MIN_PEER_REVISION,
            max_peer_revision: DEFAULT_MAX_PEER_REVISION,
            product_version: env!("CARGO_PKG_VERSION").to_string(),
            release_id: option_env!("LEGION_RELEASE_ID")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .to_string(),
            capabilities: vec![
                capabilities::AGENT_RUN_V1.to_string(),
                capabilities::SESSIONS_HISTORY_V1.to_string(),
                capabilities::APPROVAL_RESOLVE_V1.to_string(),
                capabilities::FLOWS_RUN_V1.to_string(),
                capabilities::NODES_INVOKE_V1.to_string(),
            ],
        }
    }

    /// Build a compatibility value with a specific release id and product version.
    pub fn with_release(product_version: impl Into<String>, release_id: impl Into<String>) -> Self {
        Self {
            protocol_revision: CURRENT_PROTOCOL_REVISION,
            min_peer_revision: DEFAULT_MIN_PEER_REVISION,
            max_peer_revision: DEFAULT_MAX_PEER_REVISION,
            product_version: product_version.into(),
            release_id: release_id.into(),
            capabilities: Self::current().capabilities,
        }
    }

    /// Check whether this peer and `other` can interoperate.
    ///
    /// Both revisions must fall within the other's advertised range.
    pub fn is_compatible_with(&self, other: &ProtocolCompatibility) -> bool {
        self.protocol_revision >= other.min_peer_revision
            && self.protocol_revision <= other.max_peer_revision
            && other.protocol_revision >= self.min_peer_revision
            && other.protocol_revision <= self.max_peer_revision
    }

    /// Returns the reason for incompatibility, if any.
    pub fn compatibility_error(&self, other: &ProtocolCompatibility) -> Option<String> {
        if self.is_compatible_with(other) {
            return None;
        }
        if other.protocol_revision < self.min_peer_revision {
            return Some(format!(
                "gateway protocol revision {} is below the minimum supported by this CLI ({}); upgrade the gateway",
                other.protocol_revision, self.min_peer_revision
            ));
        }
        if other.protocol_revision > self.max_peer_revision {
            return Some(format!(
                "gateway protocol revision {} is above the maximum supported by this CLI ({}); upgrade the CLI",
                other.protocol_revision, self.max_peer_revision
            ));
        }
        if self.protocol_revision < other.min_peer_revision {
            return Some(format!(
                "CLI protocol revision {} is below the minimum required by the gateway ({}); upgrade the CLI",
                self.protocol_revision, other.min_peer_revision
            ));
        }
        if self.protocol_revision > other.max_peer_revision {
            return Some(format!(
                "CLI protocol revision {} is above the maximum accepted by the gateway ({}); upgrade the gateway",
                self.protocol_revision, other.max_peer_revision
            ));
        }
        Some("protocol ranges do not overlap".to_string())
    }

    /// Check whether a capability is advertised.
    pub fn supports_capability(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_when_ranges_overlap() {
        let a = ProtocolCompatibility {
            protocol_revision: 1,
            min_peer_revision: 1,
            max_peer_revision: 2,
            product_version: "0.1.0".to_string(),
            release_id: "r1".to_string(),
            capabilities: vec![capabilities::AGENT_RUN_V1.to_string()],
        };
        let b = ProtocolCompatibility {
            protocol_revision: 2,
            min_peer_revision: 1,
            max_peer_revision: 2,
            product_version: "0.1.0".to_string(),
            release_id: "r1".to_string(),
            capabilities: vec![capabilities::AGENT_RUN_V1.to_string()],
        };
        assert!(a.is_compatible_with(&b));
        assert!(b.is_compatible_with(&a));
    }

    #[test]
    fn incompatible_when_ranges_do_not_overlap() {
        let a = ProtocolCompatibility {
            protocol_revision: 1,
            min_peer_revision: 1,
            max_peer_revision: 1,
            product_version: "0.1.0".to_string(),
            release_id: "r1".to_string(),
            capabilities: vec![],
        };
        let b = ProtocolCompatibility {
            protocol_revision: 2,
            min_peer_revision: 2,
            max_peer_revision: 2,
            product_version: "0.1.0".to_string(),
            release_id: "r1".to_string(),
            capabilities: vec![],
        };
        assert!(!a.is_compatible_with(&b));
        assert!(a.compatibility_error(&b).is_some());
    }

    #[test]
    fn supports_capability_checks_membership() {
        let mut compat = ProtocolCompatibility::current();
        compat.capabilities = vec!["a".to_string(), "b".to_string()];
        assert!(compat.supports_capability("a"));
        assert!(!compat.supports_capability("c"));
    }
}
