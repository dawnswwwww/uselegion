use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcRequest<T> {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: T,
}

impl<T> JsonRpcRequest<T> {
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: T) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse<T> {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl<T> JsonRpcResponse<T> {
    pub fn result(id: impl Into<String>, result: T) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id.into()),
            result: Some(result),
            error: None,
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

/// Agent descriptor sent to an external ACP harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    pub id: String,
    pub workspace: String,
}

/// Session descriptor sent to an external ACP harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub history: Vec<legion_provider::types::ChatMessage>,
}

/// Parameters for the `agents/run` method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunParams {
    pub agent: AgentInfo,
    pub session: SessionInfo,
    pub tools: Vec<String>,
    pub instructions: String,
    pub model: String,
}

/// Result payload returned by an `agents/run` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunResult {
    pub status: String,
    pub events: Vec<AcpEvent>,
}

/// Events emitted by an ACP harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpEvent {
    /// A text delta from the assistant.
    Text { delta: String },
    /// A tool call requested by the external harness.
    ToolCall {
        id: String,
        tool: String,
        params: serde_json::Value,
    },
    /// A tool result produced by the external harness.
    ToolResult {
        id: String,
        result: serde_json::Value,
    },
    /// The run completed.
    Done,
    /// An error occurred inside the harness.
    Error { message: String },
}

/// Parameters for the `tools/result` notification sent back to the harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultParams {
    pub id: String,
    pub result: serde_json::Value,
}

/// Known ACP methods.
pub const METHOD_AGENTS_RUN: &str = "agents/run";
pub const METHOD_TOOLS_RESULT: &str = "tools/result";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_run_request() {
        let request = JsonRpcRequest::new(
            "run-001",
            METHOD_AGENTS_RUN,
            RunParams {
                agent: AgentInfo {
                    id: "main".to_string(),
                    workspace: "~/.legion/workspace".to_string(),
                },
                session: SessionInfo {
                    id: "sess-001".to_string(),
                    history: vec![legion_provider::types::ChatMessage::user("hi")],
                },
                tools: vec!["read".to_string(), "write".to_string()],
                instructions: "Be helpful.".to_string(),
                model: "anthropic/claude-sonnet-4-6".to_string(),
            },
        );

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":\"run-001\""));
        assert!(json.contains("\"method\":\"agents/run\""));
        assert!(json.contains("\"agent\":{\"id\":\"main\",\"workspace\":\"~/.legion/workspace\"}"));
    }

    #[test]
    fn deserializes_event_stream() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": "run-001",
            "result": {
                "status": "streaming",
                "events": [
                    { "type": "text", "delta": "hello" },
                    { "type": "tool_call", "id": "call-1", "tool": "exec", "params": { "cmd": "ls" } },
                    { "type": "tool_result", "id": "call-1", "result": { "stdout": "" } },
                    { "type": "done" }
                ]
            }
        });

        let parsed: JsonRpcResponse<RunResult> = serde_json::from_value(response).unwrap();
        let result = parsed.result.unwrap();
        assert_eq!(result.status, "streaming");
        assert_eq!(result.events.len(), 4);
        assert!(matches!(result.events[0], AcpEvent::Text { .. }));
        assert!(matches!(result.events[1], AcpEvent::ToolCall { .. }));
        assert!(matches!(result.events[3], AcpEvent::Done));
    }

    #[test]
    fn deserializes_error_event() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": "run-002",
            "result": {
                "status": "error",
                "events": [
                    { "type": "error", "message": "boom" }
                ]
            }
        });

        let parsed: JsonRpcResponse<RunResult> = serde_json::from_value(response).unwrap();
        assert!(matches!(
            parsed.result.unwrap().events[0],
            AcpEvent::Error { ref message } if message == "boom"
        ));
    }
}
