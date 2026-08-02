//! Phase 0 baseline: lock the transcript and event shape produced by
//! `AgentHost::prepare_run` + `drive_run_stream`.
//!
//! The fixture covers a normal assistant-only turn. It writes to a temporary
//! transcript store and asserts both the emitted event frames and the persisted
//! JSONL transcript match the expected structure. This becomes the regression
//! guard when `drive_run_stream` is moved into `legion-host`.

use async_trait::async_trait;
use futures::stream;
use legion_core::config::Config;
use legion_gateway::{AgentHost, SessionStore, drive_run_stream};
use legion_protocol::{AgentParams, UserMessage};
use legion_provider::provider::Provider;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{
    ChatChunk, ChatRequest, ChatRole, ChatStream, EmbedRequest, Embedding, FinishReason, ModelInfo,
    ProviderError, ToolDefinition,
};
use legion_runtime::{
    AgentRuntime, MemoryBackend, MemoryError, MemoryNote, Tool, ToolRegistry, memory::MemoryMeta,
};
use std::ops::Range;
use std::sync::Arc;
use tempfile::TempDir;

/// Provider that echoes all user messages joined by commas.
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
async fn session_fixture_records_user_assistant_turn() {
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
    host.session_store = Arc::new(SessionStore::new(tmp.path()));

    let session_key = "agent:main:dm:tui:default:direct:session-fixture";
    let params = AgentParams {
        session_key: session_key.to_string(),
        message: UserMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        },
        idempotency_key: Some("run-1".to_string()),
        wait: true,
        history: Vec::new(),
        dump_prompts: false,
        yolo: false,
        workspace: None,
        sender: None,
    };

    let (stream, accepted, resolved_key) = host.prepare_run(params, None, None).await.unwrap();
    assert_eq!(accepted.run_id, "run-1");

    let mut frames: Vec<serde_json::Value> = Vec::new();
    drive_run_stream(
        stream,
        host.session_store.clone(),
        resolved_key.clone(),
        "hello".to_string(),
        accepted.run_id.clone(),
        |frame| {
            if let Ok(value) = serde_json::to_value(&frame) {
                frames.push(value);
            }
        },
    )
    .await
    .unwrap();

    // Event frame shape assertions.
    assert!(
        frames.iter().any(|f| is_lifecycle(f, "start")),
        "expected lifecycle start frame: {frames:?}"
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
    for frame in &frames {
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["event"], "agent");
        assert_eq!(frame["payload"]["run_id"], "run-1");
    }

    // Transcript persistence assertions.
    let transcript_path = tmp
        .path()
        .join("agents/main/sessions/session-fixture.jsonl");
    assert!(transcript_path.exists(), "transcript should be persisted");
    let transcript = tokio::fs::read_to_string(&transcript_path).await.unwrap();
    assert!(transcript.contains("\"role\":\"user\""));
    assert!(transcript.contains("\"content\":\"hello\""));
    assert!(transcript.contains("\"role\":\"assistant\""));

    // Resume from the transcript yields the same two messages.
    let resumed = host.session_store.load_for_resume(&resolved_key).await;
    assert_eq!(resumed.len(), 2, "expected user + assistant messages");
    assert_eq!(resumed[0].role, ChatRole::User);
    assert_eq!(resumed[0].content, "hello");
    assert_eq!(resumed[1].role, ChatRole::Assistant);
    assert_eq!(resumed[1].content, "hello");
}
