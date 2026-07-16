//! Phase 0 baseline: lock the shape of embedded (local) run events.
//!
//! This integration test drives `legion_cli::driver::run_local_turn` against a
//! fake provider and asserts that the emitted frames match the WebSocket
//! `agent` event protocol (`{"type":"event","event":"agent","payload":...}`).
//! It mirrors the in-crate `driver.rs` unit tests at the integration level so
//! the parity guarantee survives the move from `legion-gateway` to
//! `legion-host`.

use async_trait::async_trait;
use futures::stream;
use legion_cli::driver::run_local_turn;
use legion_core::config::Config;
use legion_host::AgentHost;
use legion_protocol::WsFrame;
use legion_provider::provider::Provider;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{
    ChatChunk, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding, FinishReason, ModelInfo,
    ProviderError, ToolDefinition,
};
use legion_runtime::{
    AgentRuntime, MemoryBackend, MemoryError, MemoryNote, Tool, ToolRegistry, memory::MemoryMeta,
};
use serde_json::json;
use std::ops::Range;
use std::sync::Arc;
use tempfile::TempDir;

/// Provider that echoes all prior user messages joined by commas.
struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    fn id(&self) -> &str {
        "echo"
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
        Ok(Box::pin(stream::iter(vec![Ok(ChatChunk {
            index: 0,
            delta: text,
            finish_reason: Some(FinishReason::Stop),
            tool_calls: None,
        })])))
    }

    async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
        Ok(Vec::new())
    }
}

struct EmptyToolRegistry;

impl ToolRegistry for EmptyToolRegistry {
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

    async fn index(&self, _id: &str, _content: &str, _meta: MemoryMeta) -> Result<(), MemoryError> {
        Ok(())
    }
}

fn is_lifecycle(frame: &serde_json::Value, phase: &str) -> bool {
    frame["payload"]["stream"] == "lifecycle" && frame["payload"]["phase"] == phase
}

#[tokio::test]
async fn local_turn_emits_websocket_shaped_agent_events() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let memory_path = tmp.path().join("memory");
    tokio::fs::create_dir_all(&workspace).await.unwrap();

    let config = Config::from_json(&format!(
        r#"{{
            "gateway": {{ "auth": {{ "token": "x" }} }},
            "agents": {{ "defaults": {{ "workspace": "{}", "model": "echo/gpt-4o" }} }},
            "memory": {{
                "builtin": {{
                    "collectionPath": "{}",
                    "embeddingDimension": 64
                }}
            }}
        }}"#,
        workspace.display().to_string().replace('\\', "/"),
        memory_path.display().to_string().replace('\\', "/"),
    ))
    .unwrap();

    let mut host = AgentHost::new(config.clone()).await.unwrap();
    let mut router = ProviderRouter::new();
    router.register_provider(Arc::new(EchoProvider));
    host.runtime = Arc::new(AgentRuntime::new(
        Arc::new(router),
        Arc::new(EmptyToolRegistry),
        Arc::new(FakeMemoryBackend),
        config,
    ));
    host.session_store = Arc::new(legion_host::SessionStore::new(tmp.path()));

    let frames: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = frames.clone();

    run_local_turn(
        &host,
        "agent:main:dm:cli:default:direct:parity-test",
        "hello".to_string(),
        false,
        false,
        None,
        move |frame| {
            if let WsFrame::Event {
                event_type,
                payload,
                ..
            } = frame
            {
                sink.lock()
                    .unwrap()
                    .push(json!({ "type": "event", "event": event_type, "payload": payload }));
            }
        },
    )
    .await
    .unwrap();

    let frames = frames.lock().unwrap();
    assert!(
        frames.iter().any(|f| is_lifecycle(f, "start")),
        "expected lifecycle start: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .any(|f| { f["payload"]["stream"] == "assistant" && f["payload"]["delta"] == "hello" }),
        "expected assistant delta echoing user message: {frames:?}"
    );
    assert!(
        frames.last().is_some_and(|f| is_lifecycle(f, "end")),
        "expected lifecycle end as last frame: {frames:?}"
    );
    for frame in frames.iter() {
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["event"], "agent");
        assert!(frame["payload"]["run_id"].as_str().is_some());
    }
}
