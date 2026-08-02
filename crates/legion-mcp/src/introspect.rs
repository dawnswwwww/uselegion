//! Cursor-following pagination for MCP list endpoints, capability gating,
//! and the resources/prompts introspection calls built on top of them.
//!
//! These helpers are transport-agnostic: they drive the single
//! [`McpClient::request`] primitive, so every transport (stdio, http, sse,
//! ws) gets the same behavior through the trait's default methods.

use serde_json::Value;

use crate::client::{McpClient, McpError};

/// Upper bound on pages followed for a single list call. Guards against
/// servers that never stop sending `nextCursor`.
const MAX_LIST_PAGES: usize = 100;

/// Follow `nextCursor` pagination on a list endpoint: each response may carry
/// `nextCursor`, which is sent back as `params.cursor` until absent. Returns
/// the concatenated entries of the result's `result_key` array. After
/// [`MAX_LIST_PAGES`] pages a warning is logged and the partial collection is
/// returned.
pub(crate) async fn paginate(
    client: &(impl McpClient + ?Sized),
    method: &str,
    result_key: &str,
) -> Result<Vec<Value>, McpError> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let params = match &cursor {
            Some(cursor) => serde_json::json!({ "cursor": cursor }),
            None => serde_json::json!({}),
        };
        let result = client.request(method, params).await?;
        if let Some(entries) = result.get(result_key).and_then(Value::as_array) {
            items.extend(entries.iter().cloned());
        }
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            return Ok(items);
        }
    }
    tracing::warn!(
        server = %client.server_name(),
        method = %method,
        pages = MAX_LIST_PAGES,
        "mcp list pagination exceeded page cap; returning partial results"
    );
    Ok(items)
}

/// Whether the server declared `capability` (e.g. `resources`, `prompts`)
/// during negotiation.
///
/// Servers that reported no capabilities at all (an empty object — the
/// pre-negotiation state, or older servers that omitted the field) are
/// treated permissively: the call is attempted rather than assumed
/// unsupported. Once a server declares a non-empty capability map, a missing
/// key is authoritative and gates the call off.
pub(crate) fn capability_supported(client: &(impl McpClient + ?Sized), capability: &str) -> bool {
    match client.server_capabilities().as_object() {
        Some(caps) if !caps.is_empty() => caps.contains_key(capability),
        _ => true,
    }
}

/// Error for read/get calls against a capability the server did not declare.
fn capability_unsupported(client: &(impl McpClient + ?Sized), capability: &str) -> McpError {
    McpError::Protocol(format!(
        "mcp server '{}' did not declare the '{capability}' capability",
        client.server_name()
    ))
}

/// `resources/list`, paginated. Empty when the server did not declare the
/// `resources` capability.
pub(crate) async fn list_resources(
    client: &(impl McpClient + ?Sized),
) -> Result<Vec<Value>, McpError> {
    if !capability_supported(client, "resources") {
        return Ok(Vec::new());
    }
    paginate(client, "resources/list", "resources").await
}

/// `resources/read` for a single URI. Errors when the server did not declare
/// the `resources` capability.
pub(crate) async fn read_resource(
    client: &(impl McpClient + ?Sized),
    uri: &str,
) -> Result<Value, McpError> {
    if !capability_supported(client, "resources") {
        return Err(capability_unsupported(client, "resources"));
    }
    client
        .request("resources/read", serde_json::json!({ "uri": uri }))
        .await
}

/// `prompts/list`, paginated. Empty when the server did not declare the
/// `prompts` capability.
pub(crate) async fn list_prompts(
    client: &(impl McpClient + ?Sized),
) -> Result<Vec<Value>, McpError> {
    if !capability_supported(client, "prompts") {
        return Ok(Vec::new());
    }
    paginate(client, "prompts/list", "prompts").await
}

/// `prompts/get` for a single prompt. Errors when the server did not declare
/// the `prompts` capability.
pub(crate) async fn get_prompt(
    client: &(impl McpClient + ?Sized),
    name: &str,
    arguments: Option<Value>,
) -> Result<Value, McpError> {
    if !capability_supported(client, "prompts") {
        return Err(capability_unsupported(client, "prompts"));
    }
    let params = match arguments {
        Some(arguments) => serde_json::json!({ "name": name, "arguments": arguments }),
        None => serde_json::json!({ "name": name }),
    };
    client.request("prompts/get", params).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Stub client that answers `request` from a script and records params.
    struct ScriptedListClient {
        pages: Vec<Value>,
        requests: Mutex<Vec<Value>>,
    }

    impl ScriptedListClient {
        fn new(pages: Vec<Value>) -> Self {
            Self {
                pages,
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl McpClient for ScriptedListClient {
        fn server_name(&self) -> &str {
            "scripted"
        }

        async fn connect(&self) -> Result<(), McpError> {
            Ok(())
        }

        async fn request(&self, _method: &str, params: Value) -> Result<Value, McpError> {
            let mut requests = self.requests.lock().unwrap_or_else(|p| p.into_inner());
            requests.push(params);
            let page = self
                .pages
                .get(requests.len() - 1)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            Ok(page)
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn paginate_follows_next_cursor_until_absent() {
        let client = ScriptedListClient::new(vec![
            serde_json::json!({"items": [1, 2], "nextCursor": "c2"}),
            serde_json::json!({"items": [3], "nextCursor": "c3"}),
            serde_json::json!({"items": [4]}),
        ]);
        let items = paginate(&client, "things/list", "items").await.unwrap();
        assert_eq!(
            items,
            vec![
                serde_json::json!(1),
                serde_json::json!(2),
                serde_json::json!(3),
                serde_json::json!(4)
            ]
        );
        let requests = client.requests.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(requests[0], serde_json::json!({}));
        assert_eq!(requests[1], serde_json::json!({"cursor": "c2"}));
        assert_eq!(requests[2], serde_json::json!({"cursor": "c3"}));
    }

    #[tokio::test]
    async fn paginate_stops_at_page_cap() {
        let client = ScriptedListClient::new(vec![
            serde_json::json!({"items": [1], "nextCursor": "more"});
            MAX_LIST_PAGES + 1
        ]);
        let items = paginate(&client, "things/list", "items").await.unwrap();
        assert_eq!(items.len(), MAX_LIST_PAGES);
        let requests = client.requests.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(requests.len(), MAX_LIST_PAGES);
    }

    /// Stub client with a fixed capability map.
    struct CapsClient {
        capabilities: Value,
        requested: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl McpClient for CapsClient {
        fn server_name(&self) -> &str {
            "caps"
        }

        async fn connect(&self) -> Result<(), McpError> {
            Ok(())
        }

        fn server_capabilities(&self) -> Value {
            self.capabilities.clone()
        }

        async fn request(&self, _method: &str, _params: Value) -> Result<Value, McpError> {
            *self.requested.lock().unwrap_or_else(|p| p.into_inner()) = true;
            Ok(serde_json::json!({}))
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[test]
    fn capability_gating_is_permissive_only_when_map_is_empty() {
        let unknown = CapsClient {
            capabilities: serde_json::json!({}),
            requested: Mutex::new(false),
        };
        assert!(capability_supported(&unknown, "resources"));

        let declared = CapsClient {
            capabilities: serde_json::json!({"resources": {}}),
            requested: Mutex::new(false),
        };
        assert!(capability_supported(&declared, "resources"));
        assert!(!capability_supported(&declared, "prompts"));

        let tools_only = CapsClient {
            capabilities: serde_json::json!({"tools": {}}),
            requested: Mutex::new(false),
        };
        assert!(!capability_supported(&tools_only, "resources"));
    }

    #[tokio::test]
    async fn capability_absent_short_circuits_list_and_errors_read() {
        let client = CapsClient {
            capabilities: serde_json::json!({"tools": {}}),
            requested: Mutex::new(false),
        };
        let resources = list_resources(&client).await.unwrap();
        assert!(resources.is_empty());
        assert!(!*client.requested.lock().unwrap_or_else(|p| p.into_inner()));

        let err = read_resource(&client, "file:///a").await.unwrap_err();
        match err {
            McpError::Protocol(msg) => {
                assert!(msg.contains("resources"), "got: {msg}");
                assert!(msg.contains("caps"), "got: {msg}");
            }
            other => panic!("expected protocol error, got {other:?}"),
        }
    }
}
