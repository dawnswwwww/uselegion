use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use legion_core::config::Config;
use legion_gateway::Gateway;
use legion_gateway::SessionStore;
use legion_provider::provider::Provider;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{
    ChatChunk, ChatMessage, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding,
    FinishReason, FunctionCall, ModelInfo, ProviderError, ToolDefinition,
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
    test_config_with_model(None)
}

fn test_config_with_model(model: Option<&str>) -> Config {
    let model_json = model
        .map(|m| format!("\"{}\"", m))
        .unwrap_or_else(|| "null".to_string());
    Config::from_json(&format!(
        r#"{{
            "gateway": {{ "bindHost": "127.0.0.1", "port": 18789, "auth": {{ "mode": "token", "token": "{}" }} }},
            "agents": {{ "defaults": {{ "model": {} }} }}
        }}"#,
        TOKEN, model_json
    ))
    .unwrap()
}

struct FakeToolRegistry;

impl ToolRegistry for FakeToolRegistry {
    fn get(&self, _name: &str) -> Option<Arc<dyn Tool>> {
        None
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
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

struct TextProvider {
    text: String,
}

#[async_trait]
impl Provider for TextProvider {
    fn id(&self) -> &str {
        "text"
    }

    fn supported_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let chunk = ChatChunk {
            index: 0,
            delta: self.text.clone(),
            finish_reason: Some(FinishReason::Stop),
            tool_calls: None,
        };
        Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
    }

    async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        Ok(Vec::new())
    }
}

fn fake_runtime(text: &str) -> Arc<AgentRuntime> {
    let mut router = ProviderRouter::new();
    router.register_provider(Arc::new(TextProvider {
        text: text.to_string(),
    }));

    Arc::new(AgentRuntime::new(
        Arc::new(router),
        Arc::new(FakeToolRegistry),
        Arc::new(FakeMemoryBackend),
        test_config_with_model(Some("text/gpt-4o")),
    ))
}

async fn spawn_server(
    runtime: Option<Arc<AgentRuntime>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    spawn_server_with_config(test_config(), runtime).await
}

async fn spawn_server_with_config(
    config: Config,
    runtime: Option<Arc<AgentRuntime>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    spawn_server_with_config_and_store(config, runtime, None).await
}

async fn spawn_server_with_config_and_store(
    config: Config,
    runtime: Option<Arc<AgentRuntime>>,
    store: Option<Arc<SessionStore>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let mut gateway = Gateway::new(config).await.unwrap();
    if let Some(runtime) = runtime {
        gateway = gateway.with_runtime(runtime);
    }
    if let Some(store) = store {
        gateway = gateway.with_session_store(store);
    }
    let router = gateway.router();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (addr, handle)
}

async fn connect_ws(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{}/ws", addr);
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws
}

fn parse_frame(text: &str) -> Value {
    serde_json::from_str(text).expect("valid json")
}

#[tokio::test]
#[serial]
async fn missing_connect_frame_closes_connection() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    // Send a health request before connect.
    let req = json!({
        "type": "req",
        "id": "req-1",
        "method": "health",
        "params": {}
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    // Server should close; read until close frame or stream end.
    let mut saw_close = false;
    while let Some(msg) = ws.next().await {
        match msg.unwrap() {
            Message::Close(_) => {
                saw_close = true;
                break;
            }
            Message::Text(t) => {
                let frame = parse_frame(&t);
                assert_eq!(frame["type"], "res");
                assert!(!frame["ok"].as_bool().unwrap_or(true));
            }
            _ => {}
        }
    }
    assert!(saw_close, "server should close connection");

    handle.abort();
}

#[tokio::test]
#[serial]
async fn valid_token_handshake_succeeds() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device",
            "platform": "test",
            "deviceFamily": "client",
            "role": "client"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let text = msg.into_text().unwrap();
    let frame = parse_frame(&text);

    assert_eq!(frame["type"], "res");
    assert_eq!(frame["id"], "conn-1");
    assert!(frame["ok"].as_bool().unwrap());
    assert_eq!(frame["payload"]["hello"], "ok");
    assert!(
        frame["payload"]["features"]["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("health"))
    );

    handle.abort();
}

#[tokio::test]
#[serial]
async fn handshake_hello_reports_version() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&msg);

    assert_eq!(frame["type"], "res");
    assert!(frame["ok"].as_bool().unwrap());
    assert_eq!(frame["payload"]["version"], env!("CARGO_PKG_VERSION"));

    handle.abort();
}

#[tokio::test]
#[serial]
async fn invalid_token_is_rejected() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": "wrong" },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();

    let mut rejected = false;
    while let Some(msg) = ws.next().await {
        match msg.unwrap() {
            Message::Close(_) => break,
            Message::Text(t) => {
                let frame = parse_frame(&t);
                if frame["type"] == "res" && !frame["ok"].as_bool().unwrap_or(true) {
                    rejected = true;
                }
            }
            _ => {}
        }
    }
    assert!(rejected, "invalid token should be rejected");

    handle.abort();
}

#[tokio::test]
#[serial]
async fn health_request_returns_ok() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let req = json!({
        "type": "req",
        "id": "req-h",
        "method": "health",
        "params": {}
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&resp);
    assert_eq!(frame["type"], "res");
    assert_eq!(frame["id"], "req-h");
    assert!(frame["ok"].as_bool().unwrap());
    assert_eq!(frame["payload"]["status"], "ok");

    handle.abort();
}

#[tokio::test]
#[serial]
async fn agent_request_starts_run_and_streams_events() {
    let runtime = fake_runtime("hello from agent");
    // Isolated transcript store — the default store would append test turns
    // to the real ~/.legion sessions dir.
    let temp_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(SessionStore::new(temp_dir.path()));
    let (addr, handle) = spawn_server_with_config_and_store(
        test_config_with_model(Some("text/gpt-4o")),
        Some(runtime),
        Some(store),
    )
    .await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let req = json!({
        "type": "req",
        "id": "req-a",
        "method": "agent",
        "params": {
            "sessionKey": "agent:main:dm:webchat:default:direct:user1",
            "message": { "role": "user", "content": "hi" },
            "idempotencyKey": "idem-1"
        }
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&resp);
    assert_eq!(frame["type"], "res");
    assert_eq!(frame["id"], "req-a");
    assert!(
        frame["ok"].as_bool().unwrap(),
        "agent request failed: {:?}",
        frame
    );
    assert_eq!(frame["payload"]["run_id"], "idem-1");

    // Collect streaming `agent` event frames.
    let mut events: Vec<Value> = Vec::new();
    while let Some(msg) = ws.next().await {
        let text = msg.unwrap().into_text().unwrap();
        let frame = parse_frame(&text);
        if frame["type"] == "event" && frame["event"] == "agent" {
            events.push(frame["payload"].clone());
        }
        if events.len() >= 3 {
            break;
        }
    }

    assert!(
        events
            .iter()
            .any(|e| e["stream"] == "lifecycle" && e["phase"] == "start"),
        "expected start lifecycle event"
    );
    assert!(
        events
            .iter()
            .any(|e| e["stream"] == "assistant" && e["delta"] == "hello from agent"),
        "expected assistant delta"
    );
    assert!(
        events
            .iter()
            .any(|e| e["stream"] == "lifecycle" && e["phase"] == "end"),
        "expected end lifecycle event"
    );

    handle.abort();
}

/// Provider that returns a comma-separated list of all user messages it receives.
/// This lets integration tests verify that session history is passed across turns.
struct HistoryEchoProvider;

#[async_trait]
impl Provider for HistoryEchoProvider {
    fn id(&self) -> &str {
        "history-echo"
    }

    fn supported_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
        let text = req
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join(",");
        let chunk = ChatChunk {
            index: 0,
            delta: text,
            finish_reason: Some(FinishReason::Stop),
            tool_calls: None,
        };
        Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
    }

    async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        Ok(Vec::new())
    }
}

fn history_runtime() -> Arc<AgentRuntime> {
    let mut router = ProviderRouter::new();
    router.register_provider(Arc::new(HistoryEchoProvider));

    Arc::new(AgentRuntime::new(
        Arc::new(router),
        Arc::new(FakeToolRegistry),
        Arc::new(FakeMemoryBackend),
        test_config_with_model(Some("history-echo/gpt-4o")),
    ))
}

#[tokio::test]
#[serial]
async fn agent_request_preserves_session_history() {
    let runtime = history_runtime();
    let temp_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(SessionStore::new(temp_dir.path()));
    let (addr, handle) = spawn_server_with_config_and_store(
        test_config_with_model(Some("history-echo/gpt-4o")),
        Some(runtime),
        Some(store.clone()),
    )
    .await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let session_key = "agent:main:dm:webchat:default:direct:user1";

    // First turn.
    let req1 = json!({
        "type": "req",
        "id": "req-1",
        "method": "agent",
        "params": {
            "sessionKey": session_key,
            "message": { "role": "user", "content": "first" },
            "idempotencyKey": "idem-1"
        }
    });
    ws.send(Message::Text(req1.to_string().into()))
        .await
        .unwrap();

    // Wait for the response frame and the end lifecycle event.
    let mut first_reply = None;
    loop {
        let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let frame = parse_frame(&text);
        if frame["type"] == "event" && frame["event"] == "agent" {
            if frame["payload"]["stream"] == "assistant" {
                first_reply = Some(frame["payload"]["delta"].as_str().unwrap().to_string());
            }
            if frame["payload"]["stream"] == "lifecycle" && frame["payload"]["phase"] == "end" {
                break;
            }
        }
    }
    assert_eq!(first_reply.unwrap(), "first");

    // Second turn with the same session key should include history.
    let req2 = json!({
        "type": "req",
        "id": "req-2",
        "method": "agent",
        "params": {
            "sessionKey": session_key,
            "message": { "role": "user", "content": "second" },
            "idempotencyKey": "idem-2"
        }
    });
    ws.send(Message::Text(req2.to_string().into()))
        .await
        .unwrap();

    let mut second_reply = None;
    loop {
        let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let frame = parse_frame(&text);
        if frame["type"] == "event" && frame["event"] == "agent" {
            if frame["payload"]["stream"] == "assistant" {
                second_reply = Some(frame["payload"]["delta"].as_str().unwrap().to_string());
            }
            if frame["payload"]["stream"] == "lifecycle" && frame["payload"]["phase"] == "end" {
                break;
            }
        }
    }
    assert_eq!(second_reply.unwrap(), "first,second");

    // Verify the transcript was persisted to disk.
    let transcript = store.load(session_key).await;
    assert_eq!(
        transcript.len(),
        4,
        "expected two user messages and two assistant replies"
    );
    assert_eq!(transcript[0].role, ChatRole::User);
    assert_eq!(transcript[0].content, "first");
    assert_eq!(transcript[1].role, ChatRole::Assistant);
    assert_eq!(transcript[1].content, "first");
    assert_eq!(transcript[2].role, ChatRole::User);
    assert_eq!(transcript[2].content, "second");
    assert_eq!(transcript[3].role, ChatRole::Assistant);
    assert_eq!(transcript[3].content, "first,second");

    handle.abort();
}

#[tokio::test]
#[serial]
async fn sessions_history_returns_stored_transcript() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(SessionStore::new(temp_dir.path()));
    let session_key = "agent:main:dm:tui:default:direct:user1";
    store
        .append(
            session_key,
            &[
                ChatMessage::user("first question"),
                ChatMessage::assistant("first answer"),
            ],
        )
        .await;

    let (addr, handle) = spawn_server_with_config_and_store(test_config(), None, Some(store)).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let req = json!({
        "type": "req",
        "id": "req-sh",
        "method": "sessions.history",
        "params": { "sessionKey": session_key }
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&resp);
    assert_eq!(frame["type"], "res");
    assert_eq!(frame["id"], "req-sh");
    assert!(frame["ok"].as_bool().unwrap(), "frame: {frame}");
    let messages = frame["payload"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "first question");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "first answer");

    // An unsafe peer id must be rejected, not silently answered with [].
    let bad = json!({
        "type": "req",
        "id": "req-bad",
        "method": "sessions.history",
        "params": { "sessionKey": "agent:main:dm:tui:default:direct:../evil" }
    });
    ws.send(Message::Text(bad.to_string().into()))
        .await
        .unwrap();
    let resp = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&resp);
    assert!(!frame["ok"].as_bool().unwrap_or(true));

    handle.abort();
}

#[tokio::test]
#[serial]
async fn tasks_create_and_run_round_trip() {
    let config = test_config_with_model(Some("text/gpt-4o"));
    let runtime = fake_runtime("task done");
    let (addr, handle) = spawn_server_with_config(config, Some(runtime)).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let req = json!({
        "type": "req",
        "id": "req-tc",
        "method": "tasks.create",
        "params": {
            "agent_id": "main",
            "message": "do something in background"
        }
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&resp);
    assert!(
        frame["ok"].as_bool().unwrap(),
        "tasks.create failed: {:?}",
        frame
    );
    let task_id = frame["payload"]["id"].as_str().unwrap().to_string();
    assert_eq!(frame["payload"]["status"], "pending");

    let run_req = json!({
        "type": "req",
        "id": "req-tr",
        "method": "tasks.run",
        "params": { "id": task_id }
    });
    ws.send(Message::Text(run_req.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&resp);
    assert!(
        frame["ok"].as_bool().unwrap(),
        "tasks.run failed: {:?}",
        frame
    );
    assert_eq!(frame["payload"]["id"], task_id);
    assert_eq!(
        frame["payload"]["status"], "completed",
        "task did not complete: {:?}",
        frame["payload"]["error"]
    );

    handle.abort();
}

async fn connect_node(
    addr: SocketAddr,
    node_id: &str,
    commands: Vec<&str>,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{}/ws", addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    let connect = json!({
        "type": "connect",
        "id": "conn-node",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": node_id,
            "platform": "ios",
            "deviceFamily": "phone",
            "role": "node",
            "displayName": "Test Node",
            "commands": commands,
            "capabilities": ["camera"]
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());
    ws
}

async fn send_req(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: &str,
    method: &str,
    params: Value,
) -> Value {
    let req = json!({
        "type": "req",
        "id": id,
        "method": method,
        "params": params
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        let text = msg.into_text().unwrap();
        let frame = parse_frame(&text);
        if frame["type"] == "res" && frame["id"] == id {
            return frame;
        }
    }
}

#[tokio::test]
#[serial]
async fn node_handshake_registers_node() {
    let (addr, handle) = spawn_server(None).await;
    let mut node_ws = connect_node(addr, "node-1", vec!["camera.list"]).await;

    // Connect a client and list nodes.
    let mut client_ws = connect_ws(addr).await;
    let connect = json!({
        "type": "connect",
        "id": "conn-client",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "client-1",
            "role": "client"
        }
    });
    client_ws
        .send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = client_ws
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let resp = send_req(&mut client_ws, "req-nl", "nodes.list", json!({})).await;
    assert!(
        resp["ok"].as_bool().unwrap(),
        "nodes.list failed: {:?}",
        resp
    );
    let nodes = resp["payload"]["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["id"], "node-1");
    assert_eq!(nodes[0]["display_name"], "Test Node");
    assert_eq!(nodes[0]["platform"], "ios");

    node_ws.close(None).await.unwrap();
    client_ws.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn node_status_returns_single_node() {
    let (addr, handle) = spawn_server(None).await;
    let mut node_ws = connect_node(addr, "node-status", vec!["camera.list"]).await;

    let mut client_ws = connect_ws(addr).await;
    let connect = json!({
        "type": "connect",
        "id": "conn-client",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "client-1",
            "role": "client"
        }
    });
    client_ws
        .send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = client_ws
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let resp = send_req(
        &mut client_ws,
        "req-ns",
        "nodes.status",
        json!({ "node_id": "node-status" }),
    )
    .await;
    assert!(
        resp["ok"].as_bool().unwrap(),
        "nodes.status failed: {:?}",
        resp
    );
    assert_eq!(resp["payload"]["id"], "node-status");

    let missing = send_req(
        &mut client_ws,
        "req-ns2",
        "nodes.status",
        json!({ "node_id": "missing" }),
    )
    .await;
    assert!(!missing["ok"].as_bool().unwrap());

    node_ws.close(None).await.unwrap();
    client_ws.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn node_invoke_round_trip() {
    let (addr, handle) = spawn_server(None).await;
    let mut node_ws = connect_node(addr, "node-invoke", vec!["camera.list"]).await;

    let mut client_ws = connect_ws(addr).await;
    let connect = json!({
        "type": "connect",
        "id": "conn-client",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "client-1",
            "role": "client"
        }
    });
    client_ws
        .send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = client_ws
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    // Spawn a task that answers node.invoke frames from the gateway.
    let node_answer = tokio::spawn(async move {
        while let Some(msg) = node_ws.next().await {
            let text = msg.unwrap().into_text().unwrap();
            let frame = parse_frame(&text);
            if frame["type"] == "node.invoke" {
                let correlation = frame["correlation"].as_str().unwrap();
                let res = json!({
                    "type": "node.invoke.res",
                    "correlation": correlation,
                    "response": { "photos": ["a.jpg", "b.jpg"] }
                });
                node_ws
                    .send(Message::Text(res.to_string().into()))
                    .await
                    .unwrap();
            }
        }
    });

    let resp = send_req(
        &mut client_ws,
        "req-ni",
        "node.invoke",
        json!({
            "node_id": "node-invoke",
            "command": "camera.list",
            "params": { "limit": 10 },
            "timeout_ms": 1000
        }),
    )
    .await;
    assert!(
        resp["ok"].as_bool().unwrap(),
        "node.invoke failed: {:?}",
        resp
    );
    assert_eq!(resp["payload"]["photos"], json!(["a.jpg", "b.jpg"]));

    node_answer.abort();
    client_ws.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn node_invoke_respects_policy() {
    let (addr, handle) = spawn_server(None).await;
    let mut node_ws = connect_node(addr, "node-policy", vec!["camera.snap"]).await;

    let mut client_ws = connect_ws(addr).await;
    let connect = json!({
        "type": "connect",
        "id": "conn-client",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "client-1",
            "role": "client"
        }
    });
    client_ws
        .send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = client_ws
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let resp = send_req(
        &mut client_ws,
        "req-np",
        "node.invoke",
        json!({
            "node_id": "node-policy",
            "command": "camera.snap",
            "timeout_ms": 1000
        }),
    )
    .await;
    assert!(
        !resp["ok"].as_bool().unwrap(),
        "dangerous command should be denied"
    );
    let error = resp["error"].as_str().unwrap();
    assert!(error.contains("not allowed"));

    node_ws.close(None).await.unwrap();
    client_ws.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn metrics_endpoint_exposes_prometheus_text() {
    let (addr, handle) = spawn_server(None).await;

    // Make a websocket connection so the connection counter increments.
    let mut ws = connect_ws(addr).await;
    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "metrics-device",
            "role": "client"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    // Request the metrics endpoint.
    let url = format!("http://{}/metrics", addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("text/plain"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("ws_connections_total"));
    assert!(body.contains("# TYPE ws_connections_total counter"));

    ws.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn market_list_returns_system_plugins() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "market-device",
            "role": "client"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let resp = send_req(&mut ws, "req-ml", "market.list", json!({})).await;
    assert!(
        resp["ok"].as_bool().unwrap(),
        "market.list failed: {:?}",
        resp
    );
    let plugins = resp["payload"]["plugins"].as_array().unwrap();
    assert!(!plugins.is_empty());
    assert!(plugins.iter().any(|p| p["id"] == "system:tools"));

    ws.close(None).await.unwrap();
    handle.abort();
}

#[tokio::test]
#[serial]
async fn market_install_and_uninstall_round_trip() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "market-device",
            "role": "client"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let install = send_req(
        &mut ws,
        "req-mi",
        "market.install",
        json!({ "id": "system:tools" }),
    )
    .await;
    assert!(
        install["ok"].as_bool().unwrap(),
        "market.install failed: {:?}",
        install
    );
    assert_eq!(install["payload"]["installed"], true);

    let list = send_req(&mut ws, "req-ml2", "market.list", json!({})).await;
    let tools = list["payload"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "system:tools")
        .cloned()
        .unwrap();
    assert_eq!(tools["installed"], true);

    let uninstall = send_req(
        &mut ws,
        "req-mu",
        "market.uninstall",
        json!({ "id": "system:tools" }),
    )
    .await;
    assert!(uninstall["ok"].as_bool().unwrap());

    let unknown = send_req(
        &mut ws,
        "req-mu2",
        "market.uninstall",
        json!({ "id": "unknown" }),
    )
    .await;
    assert!(!unknown["ok"].as_bool().unwrap());

    ws.close(None).await.unwrap();
    handle.abort();
}

// ---- interactive tool approval over the gateway websocket ----

/// Provider that emits an `exec` tool call on its first request and a final
/// text answer once a tool result is in the conversation.
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
                        arguments: r#"{"command":"ls"}"#.into(),
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

/// An `exec` tool with the real tool's default `Approval::Required` policy.
struct RequiredExecTool;

fn required_policy() -> &'static Policy {
    use std::sync::OnceLock;
    static POLICY: OnceLock<Policy> = OnceLock::new();
    POLICY.get_or_init(|| Policy {
        approval: Approval::Required,
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
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } }
        })
    }

    fn policy(&self) -> &Policy {
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
        if name == "exec" {
            Some(Arc::new(RequiredExecTool))
        } else {
            None
        }
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "exec".to_string(),
            description: "exec".to_string(),
            parameters: json!({ "type": "object" }),
        }]
    }
}

fn approval_runtime() -> Arc<AgentRuntime> {
    let mut router = ProviderRouter::new();
    router.register_provider(Arc::new(ToolCallProvider));
    Arc::new(AgentRuntime::new(
        Arc::new(router),
        Arc::new(ExecToolRegistry),
        Arc::new(FakeMemoryBackend),
        test_config_with_model(Some("tool-call/gpt-4o")),
    ))
}

#[tokio::test]
#[serial]
async fn agent_run_prompts_for_approval_and_resolves_via_ws() {
    let runtime = approval_runtime();
    // Isolate the transcript: the default store would read/write the real
    // ~/.legion sessions dir, and a resumed history already containing a tool
    // result flips ToolCallProvider straight to its final answer — the run
    // would never prompt for approval.
    let temp_dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(SessionStore::new(temp_dir.path()));
    let (addr, handle) = spawn_server_with_config_and_store(
        test_config_with_model(Some("tool-call/gpt-4o")),
        Some(runtime),
        Some(store),
    )
    .await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let req = json!({
        "type": "req",
        "id": "req-a",
        "method": "agent",
        "params": {
            "sessionKey": "agent:main:dm:tui:default:direct:user1",
            "message": { "role": "user", "content": "run ls" },
            "idempotencyKey": "idem-approval"
        }
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    // The accepted response arrives first; the run streams events after it.
    let resp = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let frame = parse_frame(&resp);
    assert!(
        frame["ok"].as_bool().unwrap(),
        "agent request failed: {frame}"
    );

    // The exec tool is Approval::Required: instead of hanging silently, the
    // gateway must stream an `approval` event carrying the prompt id.
    let prompt_id = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let frame = parse_frame(&text);
            if frame["type"] == "event" && frame["event"] == "approval" {
                assert_eq!(frame["payload"]["tool"], "exec");
                break frame["payload"]["promptId"]
                    .as_str()
                    .expect("promptId")
                    .to_string();
            }
        }
    })
    .await
    .expect("timed out waiting for the approval event");

    // Approve via the new RPC. Send the request manually rather than via
    // send_req: the fake tool completes instantly, so the run's completion
    // events race with the ack — send_req would discard them while scanning
    // for its response, and the final wait would never see lifecycle/end.
    let req = json!({
        "type": "req",
        "id": "req-approve",
        "method": "approval.resolve",
        "params": { "promptId": prompt_id, "allow": true }
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    // Collect until the run finishes: the resolve ack, tool result, final
    // assistant answer, and lifecycle/end may interleave in any order.
    let mut acked = false;
    let mut tool_ok = false;
    let mut answered = false;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let frame = parse_frame(&text);
            if frame["type"] == "res" && frame["id"] == "req-approve" {
                assert!(
                    frame["ok"].as_bool().unwrap(),
                    "approval.resolve failed: {frame}"
                );
                assert_eq!(frame["payload"]["resolved"], true);
                acked = true;
            }
            if frame["type"] == "event" && frame["event"] == "agent" {
                let payload = &frame["payload"];
                if payload["stream"] == "tool"
                    && payload["state"] == "end"
                    && payload["result"]["is_error"] == false
                {
                    tool_ok = true;
                }
                if payload["stream"] == "assistant" && payload["delta"] == "done" {
                    answered = true;
                }
                if payload["stream"] == "lifecycle" && payload["phase"] == "end" {
                    break;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for the run to finish after approval");
    assert!(acked, "approval.resolve ack must arrive");
    assert!(tool_ok, "approved tool must execute successfully");
    assert!(answered, "run must finish with the post-tool answer");

    handle.abort();
}

#[tokio::test]
#[serial]
async fn approval_resolve_unknown_prompt_is_rejected() {
    let (addr, handle) = spawn_server(None).await;
    let mut ws = connect_ws(addr).await;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device"
        }
    });
    ws.send(Message::Text(connect.to_string().into()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(parse_frame(&hello)["ok"].as_bool().unwrap());

    let resp = send_req(
        &mut ws,
        "req-unknown",
        "approval.resolve",
        json!({ "promptId": "prompt-999", "allow": true }),
    )
    .await;
    assert!(!resp["ok"].as_bool().unwrap_or(true));
    assert!(
        resp["error"].as_str().unwrap_or("").contains("prompt-999"),
        "error should name the unknown prompt: {resp}"
    );

    handle.abort();
}
