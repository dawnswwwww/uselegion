use async_trait::async_trait;
use futures::StreamExt;
use legion_acp::harness::AcpHarness;
use legion_core::config::Config;
use legion_provider::types::ToolDefinition;
use legion_runtime::tools::{Approval, Policy, ToolDefinitionExt};
use legion_runtime::{
    Harness, HarnessRegistry, LifecyclePhase, RunEvent, RunRequest, RunStream, Tool, ToolContext,
    ToolError, ToolRegistry, ToolResult,
};
use legion_runtime::{MemoryBackend, MemoryError, MemoryNote};

fn open_policy() -> &'static Policy {
    use std::sync::OnceLock;
    static POLICY: OnceLock<Policy> = OnceLock::new();
    POLICY.get_or_init(|| Policy {
        approval: Approval::Off,
        permission_mode: None,
        allow_from: vec![],
        workspace_only: false,
    })
}
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the provided message."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "msg": { "type": "string" }
            },
            "required": ["msg"]
        })
    }

    fn policy(&self) -> &Policy {
        open_policy()
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let msg = params["msg"].as_str().unwrap_or("");
        Ok(ToolResult::ok(format!("echo: {msg}")))
    }
}

struct EchoRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl EchoRegistry {
    fn new() -> Self {
        let mut tools = HashMap::new();
        tools.insert("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>);
        Self { tools }
    }
}

impl ToolRegistry for EchoRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
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

fn test_config() -> Config {
    Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap()
}

fn mock_binary() -> String {
    std::env::var("CARGO_BIN_EXE_mock-acp-harness")
        .expect("CARGO_BIN_EXE_mock-acp-harness must be set by cargo")
}

async fn collect_events(mut stream: RunStream) -> Vec<RunEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn acp_harness_runs_mock_without_tool_call() {
    let harness = AcpHarness::new(
        vec![mock_binary()],
        Arc::new(EchoRegistry::new()),
        test_config(),
    );
    let request = RunRequest::new("session-1", "main", "just chat", "acp:mock");
    let events = collect_events(harness.run(request).unwrap()).await;

    assert_eq!(
        events[0],
        RunEvent::Lifecycle {
            phase: LifecyclePhase::Start,
            error: None
        }
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::AssistantDelta { delta } if delta == "done")),
        "expected assistant delta 'done', got {:?}",
        events
    );
    assert!(
        events.iter().any(
            |e| matches!(e, RunEvent::Lifecycle { phase, .. } if *phase == LifecyclePhase::End)
        ),
        "expected end lifecycle event"
    );
}

#[tokio::test]
async fn acp_harness_round_trips_tool_call() {
    let harness = AcpHarness::new(
        vec![mock_binary()],
        Arc::new(EchoRegistry::new()),
        test_config(),
    );
    let request = RunRequest::new("session-1", "main", "call echo", "acp:mock");
    let events = collect_events(harness.run(request).unwrap()).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            RunEvent::ToolStart { tool_call } if tool_call.name == "echo"
        )),
        "expected tool start"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            RunEvent::ToolEnd { tool_call, result, .. }
            if tool_call.name == "echo" && result.content == "echo: hello"
        )),
        "expected tool end with echo result"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            RunEvent::AssistantDelta { delta } if delta == "done"
        )),
        "expected final assistant delta"
    );
}

#[test]
fn registry_selects_harness_by_model_ref() {
    let mut registry = HarnessRegistry::new();
    registry.register(Arc::new(legion_runtime::AgentRuntime::new(
        Arc::new(legion_provider::router::ProviderRouter::new()),
        Arc::new(EchoRegistry::new()),
        Arc::new(FakeMemoryBackend),
        test_config(),
    )));
    registry.register(Arc::new(AcpHarness::new(
        vec![mock_binary()],
        Arc::new(EchoRegistry::new()),
        test_config(),
    )));

    assert_eq!(registry.select("openai/gpt-4o").unwrap().id(), "built-in");
    assert_eq!(registry.select("acp:mock").unwrap().id(), "acp");
}

#[test]
fn registry_default_selects_acp_for_all_models() {
    let mut registry = HarnessRegistry::new();
    registry.register(Arc::new(legion_runtime::AgentRuntime::new(
        Arc::new(legion_provider::router::ProviderRouter::new()),
        Arc::new(EchoRegistry::new()),
        Arc::new(FakeMemoryBackend),
        test_config(),
    )));
    registry.register(Arc::new(AcpHarness::new(
        vec![mock_binary()],
        Arc::new(EchoRegistry::new()),
        test_config(),
    )));
    let registry = registry.with_default("acp");

    assert_eq!(registry.select("openai/gpt-4o").unwrap().id(), "acp");
    assert_eq!(registry.select("acp:mock").unwrap().id(), "acp");
}
