//! Shared WebSocket wire types used by both the Gateway and the CLI.
//!
//! These DTOs are intentionally transport-agnostic once serialized: the
//! protocol crate only owns the JSON shape, not socket I/O or dispatch.

use crate::ProtocolCompatibility;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// A WebSocket protocol frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum WsFrame {
    Connect {
        id: String,
        #[serde(default)]
        params: ConnectParams,
    },
    Req {
        id: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Res {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Event {
        #[serde(rename = "event")]
        event_type: String,
        payload: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub auth: AuthCreds,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub device_family: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub permissions: Option<HashMap<String, bool>>,
    /// Optional protocol compatibility advertised by the connecting peer.
    /// Gateways that do not understand this field ignore it safely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<crate::ProtocolCompatibility>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthCreds {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl WsFrame {
    pub fn ok(id: impl Into<String>, payload: impl Serialize) -> Self {
        Self::Res {
            id: id.into(),
            ok: true,
            payload: serde_json::to_value(payload).ok(),
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Res {
            id: id.into(),
            ok: false,
            payload: None,
            error: Some(error.into()),
        }
    }

    pub fn event(event_type: impl Into<String>, payload: impl Serialize) -> Self {
        Self::Event {
            event_type: event_type.into(),
            payload: serde_json::to_value(payload).unwrap_or_default(),
            seq: None,
        }
    }

    /// Replace the id of a `Res` frame; other variants pass through unchanged.
    pub fn with_id(self, id: &str) -> Self {
        match self {
            Self::Res {
                ok, payload, error, ..
            } => Self::Res {
                id: id.to_string(),
                ok,
                payload,
                error,
            },
            other => other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloPayload {
    pub hello: String,
    pub gateway_id: String,
    /// Crate version of the gateway, so older CLI clients can still detect a
    /// stale background gateway. New code should prefer `protocol`.
    pub version: String,
    /// Machine-readable protocol compatibility information. Added in protocol
    /// revision 1; older clients that only check `version` continue to work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProtocolCompatibility>,
    pub features: Features,
    pub snapshot: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Features {
    pub methods: Vec<String>,
    pub events: Vec<String>,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            methods: vec![
                "health".to_string(),
                "status".to_string(),
                "send".to_string(),
                "agent".to_string(),
                "system-presence".to_string(),
                "nodes.list".to_string(),
                "nodes.status".to_string(),
                "node.invoke".to_string(),
                "market.list".to_string(),
                "market.install".to_string(),
                "market.uninstall".to_string(),
            ],
            events: vec![
                "tick".to_string(),
                "agent".to_string(),
                "approval".to_string(),
                "question".to_string(),
                "presence".to_string(),
                "shutdown".to_string(),
                "node.invoke".to_string(),
                "node.invoke.res".to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ws_frame_event_round_trip() {
        let frame = WsFrame::event("agent", json!({"stream": "assistant", "delta": "hi"}));
        let json = serde_json::to_string(&frame).unwrap();
        let parsed: WsFrame = serde_json::from_str(&json).unwrap();
        match parsed {
            WsFrame::Event {
                event_type,
                payload,
                seq,
            } => {
                assert_eq!(event_type, "agent");
                assert_eq!(payload["delta"], "hi");
                assert!(seq.is_none());
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn ws_frame_res_ok_round_trip() {
        let frame = WsFrame::ok("req-1", json!({"messages": []}));
        let parsed: WsFrame =
            serde_json::from_str(&serde_json::to_string(&frame).unwrap()).unwrap();
        match parsed {
            WsFrame::Res {
                id,
                ok,
                payload,
                error,
            } => {
                assert_eq!(id, "req-1");
                assert!(ok);
                assert_eq!(payload.unwrap()["messages"], json!([]));
                assert!(error.is_none());
            }
            other => panic!("expected Res, got {other:?}"),
        }
    }

    #[test]
    fn hello_payload_serializes_with_camel_case() {
        let hello = HelloPayload {
            hello: "legion".to_string(),
            gateway_id: "gw-1".to_string(),
            version: "0.1.0".to_string(),
            protocol: None,
            features: Features::default(),
            snapshot: json!({}),
        };
        let value = serde_json::to_value(&hello).unwrap();
        assert_eq!(value["gateway_id"], "gw-1");
        assert!(value["features"]["methods"].is_array());
    }
}
