use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// A paired/known device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub device_id: String,
    pub platform: String,
    pub device_family: String,
    pub role: String,
    pub approved: bool,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
}

/// Device pairing store.
#[derive(Default, Clone)]
pub struct PairingStore {
    inner: Arc<Mutex<PairingState>>,
}

#[derive(Default)]
struct PairingState {
    devices: HashMap<String, Device>,
    pending: Vec<String>,
    token_counter: u64,
}

impl PairingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if the connecting address is loopback.
    pub fn is_loopback(addr: SocketAddr) -> bool {
        addr.ip().is_loopback()
    }

    /// Determine whether a device is already approved.
    pub fn is_approved(&self, device_id: &str) -> bool {
        let state = self.inner.lock().unwrap();
        state
            .devices
            .get(device_id)
            .map(|d| d.approved)
            .unwrap_or(false)
    }

    /// Look up a device by id.
    pub fn get_device(&self, device_id: &str) -> Option<Device> {
        let state = self.inner.lock().unwrap();
        state.devices.get(device_id).cloned()
    }

    /// Verify a device token.
    pub fn verify_token(&self, device_id: &str, token: &str) -> bool {
        let state = self.inner.lock().unwrap();
        state
            .devices
            .get(device_id)
            .map(|d| d.approved && d.token == token)
            .unwrap_or(false)
    }

    /// Approve a device explicitly, issuing a persistent device token.
    pub fn approve(&self, device_id: impl Into<String>) -> String {
        let mut state = self.inner.lock().unwrap();
        let device_id = device_id.into();
        state.token_counter += 1;
        let token = format!("dev-token-{}", state.token_counter);
        state.pending.retain(|id| id != &device_id);
        state.devices.insert(
            device_id.clone(),
            Device {
                device_id,
                platform: String::new(),
                device_family: String::new(),
                role: String::new(),
                approved: true,
                token: token.clone(),
                approved_at: None,
            },
        );
        token
    }

    /// Record a new device as pending approval (non-loopback, unknown device).
    pub fn request_approval(&self, device: Device) {
        let mut state = self.inner.lock().unwrap();
        if !state.devices.contains_key(&device.device_id)
            && !state.pending.contains(&device.device_id)
        {
            state.pending.push(device.device_id.clone());
        }
        state
            .devices
            .entry(device.device_id.clone())
            .or_insert(device);
    }

    /// Auto-approve loopback connections, returning the issued token.
    pub fn auto_approve_loopback(&self, device: Device) -> String {
        self.approve(device.device_id)
    }

    /// List pending device ids awaiting explicit approval.
    pub fn pending_approvals(&self) -> Vec<String> {
        let state = self.inner.lock().unwrap();
        state.pending.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(device_id: &str, approved: bool, token: &str) -> Device {
        Device {
            device_id: device_id.to_string(),
            platform: String::new(),
            device_family: String::new(),
            role: String::new(),
            approved,
            token: token.to_string(),
            approved_at: None,
        }
    }

    #[test]
    fn verify_token_requires_approved_and_matching_token() {
        let store = PairingStore::new();

        // Known device with a token, but not approved: verification fails.
        store.request_approval(device("unapproved", false, "T"));
        assert!(!store.verify_token("unapproved", "T"));

        // Approved device with the matching token verifies; a wrong token
        // does not.
        store.request_approval(device("approved", true, "T"));
        assert!(store.verify_token("approved", "T"));
        assert!(!store.verify_token("approved", "wrong"));

        // Unknown devices never verify.
        assert!(!store.verify_token("ghost", "T"));
    }

    #[test]
    fn request_approval_dedups_pending() {
        let store = PairingStore::new();
        store.request_approval(device("dev-1", false, ""));
        store.request_approval(device("dev-1", false, ""));
        assert_eq!(store.pending_approvals(), vec!["dev-1".to_string()]);

        // A different device gets its own pending entry.
        store.request_approval(device("dev-2", false, ""));
        assert_eq!(
            store.pending_approvals(),
            vec!["dev-1".to_string(), "dev-2".to_string()]
        );
    }

    #[test]
    fn approve_removes_from_pending_and_rotates_token() {
        let store = PairingStore::new();
        store.request_approval(device("dev-1", false, "old-token"));
        assert_eq!(store.pending_approvals(), vec!["dev-1".to_string()]);

        let token = store.approve("dev-1");
        assert!(store.pending_approvals().is_empty());
        // Fresh store: the token counter starts at 0, so the first issued
        // token is `dev-token-1`.
        assert_eq!(token, "dev-token-1");

        let stored = store.get_device("dev-1").unwrap();
        assert!(stored.approved);
        assert_eq!(stored.token, token);
        assert_ne!(stored.token, "old-token");

        // A second approval rotates the token again.
        let rotated = store.approve("dev-1");
        assert_eq!(rotated, "dev-token-2");
        assert_ne!(rotated, token);
        assert!(store.verify_token("dev-1", &rotated));
        assert!(!store.verify_token("dev-1", &token));
    }

    #[test]
    fn is_loopback_detects_loopback_addresses() {
        for addr in ["127.0.0.1:1234", "[::1]:8080"] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert!(PairingStore::is_loopback(addr), "{addr} is loopback");
        }
        for addr in ["0.0.0.0:80", "192.168.1.1:443"] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert!(!PairingStore::is_loopback(addr), "{addr} is not loopback");
        }
    }
}
