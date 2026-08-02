//! Minimal shared JSON-RPC 2.0 message helpers.
//!
//! Used by the MCP, LSP and ACP clients, which differ in framing (newline
//! delimited, HTTP, WebSocket, SSE) but build and parse the same message
//! objects. Framing stays per-transport; only the message shape lives here.

use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

/// Error object carried by a JSON-RPC error response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("server returned error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Allocate the next request id from a monotonically increasing counter.
pub fn next_id(counter: &AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::Relaxed)
}

/// Build a JSON-RPC 2.0 request object.
pub fn build_request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Build a JSON-RPC 2.0 notification object (no id, no response expected).
pub fn build_notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// Extract the `result` of a JSON-RPC response, or fail with its `error`
/// object. A missing `result` decodes as [`Value::Null`].
pub fn parse_result(msg: &Value) -> Result<Value, RpcError> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string();
        return Err(RpcError { code, message });
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_carries_id_method_params() {
        let req = build_request(7, "tools/list", json!({}));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        assert_eq!(req["method"], "tools/list");
        assert_eq!(req["params"], json!({}));
    }

    #[test]
    fn notification_has_no_id() {
        let msg = build_notification("notifications/initialized", json!({}));
        assert_eq!(msg["method"], "notifications/initialized");
        assert!(msg.get("id").is_none());
    }

    #[test]
    fn ids_increase_monotonically() {
        let counter = AtomicU64::new(1);
        assert_eq!(next_id(&counter), 1);
        assert_eq!(next_id(&counter), 2);
    }

    #[test]
    fn parse_result_extracts_result_and_error() {
        let ok = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        assert_eq!(parse_result(&ok).unwrap(), json!({"tools": []}));

        // A response without a result decodes as null.
        let bare = json!({"jsonrpc": "2.0", "id": 1});
        assert_eq!(parse_result(&bare).unwrap(), Value::Null);

        let err = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "method not found"}});
        let rpc = parse_result(&err).unwrap_err();
        assert_eq!(rpc.code, -32601);
        assert_eq!(rpc.message, "method not found");

        // Malformed error objects fall back to -1 / "unknown error".
        let weird = json!({"error": true});
        let rpc = parse_result(&weird).unwrap_err();
        assert_eq!(rpc.code, -1);
        assert_eq!(rpc.message, "unknown error");
    }
}
