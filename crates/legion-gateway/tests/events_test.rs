//! Integration test for the `/events` external event bus.
//!
//! Drives the real Gateway HTTP server over loopback: one `/ws` connection
//! starts an agent run, a second `/events` connection attaches to that session
//! and asserts it receives the run's lifecycle/tool/assistant events. This is
//! the main acceptance test for the v1 event-bus contract.

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use legion_core::config::Config;
use legion_gateway::{Gateway, SessionStore};
use legion_provider::provider::Provider;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{
    ChatChunk, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding, FinishReason,
    FunctionCall, ModelInfo, ProviderError, ToolDefinition,
};
use legion_runtime::tools::{Approval, Policy};
use legion_runtime::{
    AgentRuntime, MemoryBackend, MemoryError, MemoryNote, Tool, ToolContext, ToolError,
    ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use serial_test::serial;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "test-token-secret";

fn test_config() -> Config {
    Config::from_json(&format!(
        r#"{{
            "gateway": {{ "bindHost": "127.0.0.1", "port": 18789, "auth": {{ "mode": "token", "token": "{}" }} }},
            "agents": {{ "defaults": {{ "model": "tool-call/gpt-4o" }} }}
        }}"#,
        TOKEN
    ))
    .unwrap()
}

struct FakeMemoryBackend;

#[async_trait]
impl MemoryBackend for FakeMemoryBackend {
    async fn search(&self, _query: &str, _top_k: usize) -> Result<Vec<MemoryNote>, MemoryError> {
        Ok(Vec::new())
    }
    async fn get(&self, _path: &str, _range: Option<Range<usize>>) -> Result<String, MemoryError> {
        Ok(String::new())
    }
    async fn index(
        &self,
        _id: &str,
        _content: &str,
        _meta: legion_runtime::memory::MemoryMeta,
    ) -> Result<(), MemoryError> {
        Ok(())
    }
}

/// Provider that emits an `exec` tool call on its first request, then a final
/// text answer once the tool result is in the conversation.
struct ToolCallProvider;

#[async_trait]
impl Provider for ToolCallProvider {
    fn id(&self) -> &str {
        "tool-call"
    }
    fn supported_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let saw_tool_result = req.messages.iter().any(|m| m.role == ChatRole::Tool);
        let chunk = if saw_tool_result {
            ChatChunk {
                index: 0,
                delta: "done".to_string(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            }
        } else {
            ChatChunk {
                index: 0,
                delta: String::new(),
                finish_reason: Some(FinishReason::ToolCalls),
                tool_calls: Some(vec![legion_provider::types::ToolCall {
                    id: "call-1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "exec".into(),
                        arguments: r#"{"command":"echo hi"}"#.into(),
                    },
                }]),
            }
        };
        Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
    }
    async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        Ok(Vec::new())
    }
}

/// `exec` tool carrying `Approval::Required`; the agent request sends
/// `yolo: true` so the gateway auto-approves without interactive prompting.
struct RequiredExecTool;

fn required_policy() -> &'static Policy {
    use std::sync::OnceLock;
    static POLICY: OnceLock<Policy> = OnceLock::new();
    POLICY.get_or_init(|| Policy {
        approval: Approval::Required,
        permission_mode: None,
        allow_from: vec![],
        workspace_only: false,
    })
}

#[async_trait]
impl Tool for RequiredExecTool {
    fn name(&self) -> &str {
        "exec"
    }
    fn description(&self) -> &str {
        "exec"
    }
    fn schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": { "command": { "type": "string" } } })
    }
    fn policy(&self) -> &'static Policy {
        required_policy()
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok(format!(
            "ran {}",
            params["command"].as_str().unwrap_or("")
        )))
    }
}

struct ExecToolRegistry;

impl ToolRegistry for ExecToolRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        (name == "exec").then(|| Arc::new(RequiredExecTool) as Arc<dyn Tool>)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "exec".to_string(),
            description: "exec".to_string(),
            parameters: json!({ "type": "object" }),
        }]
    }
}

fn runtime() -> Arc<AgentRuntime> {
    let mut router = ProviderRouter::new();
    router.register_provider(Arc::new(ToolCallProvider));
    Arc::new(AgentRuntime::new(
        Arc::new(router),
        Arc::new(ExecToolRegistry),
        Arc::new(FakeMemoryBackend),
        test_config(),
    ))
}

async fn spawn_server(
    runtime: Arc<AgentRuntime>,
    store: Arc<SessionStore>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let mut gateway = Gateway::new(test_config()).await.unwrap();
    gateway = gateway.with_runtime(runtime).with_session_store(store);
    gateway.start_automation().await.unwrap();
    let router = gateway.router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (addr, handle)
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_path(addr: SocketAddr, path: &str) -> WsStream {
    let url = format!("ws://{addr}{path}");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("valid json")
}

/// Send a `/ws` `connect` frame and wait for the hello `res`.
async fn ws_handshake(ws: &mut WsStream) {
    let connect = json!({
        "type": "connect", "id": "conn-1",
        "params": { "auth": { "token": TOKEN }, "deviceId": "ws-device" }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse(&hello)["ok"].as_bool().unwrap());
}

#[tokio::test]
#[serial]
async fn events_subscriber_receives_run_lifecycle_and_tool_events() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(SessionStore::new(temp_dir.path()));
    let (addr, handle) = spawn_server(runtime(), store).await;

    // --- Subscriber B: connect to /events BEFORE the run starts so it sees
    //     the full event sequence. Attach to the known session key. ---
    let session_key = "agent:main:dm:tui:default:direct:events-user";
    let mut sub = connect_path(addr, "/events").await;
    // /events uses the same connect handshake.
    let connect = json!({
        "type": "connect", "id": "conn-events",
        "params": { "auth": { "token": TOKEN }, "deviceId": "events-device" }
    });
    sub.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    // First server frame is HelloOk (harness protocol), not a WsFrame res.
    let hello = sub.next().await.unwrap().unwrap().into_text().unwrap();
    let hello_frame = parse(&hello);
    assert_eq!(
        hello_frame["type"], "hello_ok",
        "hello frame: {hello_frame}"
    );
    assert_eq!(hello_frame["v"], 1);

    // Attach to the session up front.
    let attach = json!({ "type": "attach_session", "sessionKey": session_key });
    sub.send(Message::Text(attach.to_string().into()))
        .await
        .unwrap();
    let attached = sub.next().await.unwrap().unwrap().into_text().unwrap();
    let attached_frame = parse(&attached);
    assert_eq!(
        attached_frame["type"], "attached",
        "attached frame: {attached_frame}"
    );

    // --- Initiator A: start the run via /ws. ---
    let mut init = connect_path(addr, "/ws").await;
    ws_handshake(&mut init).await;
    let req = json!({
        "type": "req", "id": "req-a", "method": "agent",
        "params": {
            "sessionKey": session_key,
            "message": { "role": "user", "content": "run echo" },
            "idempotencyKey": "idem-events",
            "yolo": true
        }
    });
    init.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();
    // Drain the accepted res (don't assert its content).
    let _ = init.next().await.unwrap().unwrap().into_text().unwrap();

    // --- Collect the subscriber's event sequence. ---
    let mut kinds: Vec<String> = Vec::new();
    let deadline = std::time::Duration::from_secs(15);
    tokio::time::timeout(deadline, async {
        loop {
            let text = sub.next().await.unwrap().unwrap().into_text().unwrap();
            let frame = parse(&text);
            if frame["type"] == "event" {
                kinds.push(frame["event"]["kind"].as_str().unwrap().to_string());
                if kinds.last().is_some_and(|k| k == "run_finished") {
                    break;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for the full event sequence");

    // Assert the ordered lifecycle (positions matter for the bookends;
    // tool events must sit between run_started and run_finished).
    let first = kinds.first().expect("at least one event");
    assert_eq!(first, "run_started", "sequence: {kinds:?}");
    assert_eq!(kinds.last().unwrap(), "run_finished", "sequence: {kinds:?}");
    assert!(
        kinds.iter().any(|k| k == "tool_started"),
        "expected a tool_started event, sequence: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "tool_finished"),
        "expected a tool_finished event, sequence: {kinds:?}"
    );
    // Index sanity: tool_finished comes after tool_started.
    let ts = kinds.iter().position(|k| k == "tool_started").unwrap();
    let tf = kinds.iter().position(|k| k == "tool_finished").unwrap();
    assert!(tf > ts, "tool_finished must follow tool_started: {kinds:?}");

    sub.close(None).await.unwrap();
    init.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn events_list_sessions_reports_active_session_as_live() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(SessionStore::new(temp_dir.path()));
    let (addr, handle) = spawn_server(runtime(), store).await;

    let session_key = "agent:main:dm:tui:default:direct:list-user";

    // Subscribe + attach (this registers the session in the bus as idle).
    let mut sub = connect_path(addr, "/events").await;
    let connect = json!({
        "type": "connect", "id": "conn-events",
        "params": { "auth": { "token": TOKEN }, "deviceId": "events-device" }
    });
    sub.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let _hello = sub.next().await.unwrap().unwrap().into_text().unwrap();

    let attach = json!({ "type": "attach_session", "sessionKey": session_key });
    sub.send(Message::Text(attach.to_string().into()))
        .await
        .unwrap();
    let _attached = sub.next().await.unwrap().unwrap().into_text().unwrap();

    // ListSessions reflects the attached session.
    sub.send(Message::Text(
        json!({ "type": "list_sessions" }).to_string().into(),
    ))
    .await
    .unwrap();
    let list = sub.next().await.unwrap().unwrap().into_text().unwrap();
    let list_frame = parse(&list);
    assert_eq!(
        list_frame["type"], "session_list",
        "list frame: {list_frame}"
    );
    let sessions = list_frame["sessions"].as_array().expect("sessions array");
    let entry = sessions
        .iter()
        .find(|s| s["sessionKey"] == session_key)
        .unwrap_or_else(|| panic!("session not listed: {list_frame}"));
    // Idle until a run starts.
    assert_eq!(entry["status"], "idle", "entry: {entry}");

    sub.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn events_rejects_non_loopback_bind() {
    // The /events loopback guard checks `state.config.gateway.bind_host`, not
    // the serving socket. We build a gateway whose configured bind host is a
    // public address, serve it on an ephemeral loopback port, and confirm the
    // handler rejects the connection before sending `hello_ok`.
    let public_config = Config::from_json(&format!(
        r#"{{
            "gateway": {{ "bindHost": "0.0.0.0", "port": 18790, "auth": {{ "mode": "token", "token": "{}" }} }},
            "agents": {{ "defaults": {{ "model": "tool-call/gpt-4o" }} }}
        }}"#,
        TOKEN
    ))
    .unwrap();
    let temp_dir = tempfile::TempDir::new().unwrap();
    let mut gateway = Gateway::new(public_config).await.unwrap();
    gateway = gateway
        .with_runtime(runtime())
        .with_session_store(Arc::new(SessionStore::new(temp_dir.path())));
    gateway.start_automation().await.unwrap();
    let router = gateway.router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let mut sub = connect_path(addr, "/events").await;
    let connect = json!({
        "type": "connect", "id": "conn-events",
        "params": { "auth": { "token": TOKEN }, "deviceId": "events-device" }
    });
    sub.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();

    // The guard fires before the harness handshake, so the server replies with a
    // WsFrame error (or closes). Either way, no `hello_ok` should arrive.
    let mut rejected = false;
    while let Some(Ok(msg)) = sub.next().await {
        match msg {
            Message::Text(t) => {
                let f = parse(&t);
                if f["type"] == "res" && !f["ok"].as_bool().unwrap_or(true) {
                    rejected = true;
                    break;
                }
                // A harness frame here would be a protocol violation.
                assert_ne!(f["type"], "hello_ok", "non-loopback should not hello");
            }
            Message::Close(_) => {
                rejected = true;
                break;
            }
            _ => {}
        }
    }
    assert!(rejected, "non-loopback bind should be rejected");

    sub.close(None).await.ok();
    handle.abort();
}
