//! Turn drivers: how a CLI turn reaches an agent runtime.
//!
//! The TUI and `legion agent` talk to the runtime through a [`TurnDriver`]
//! so the event/rendering layer never has to know whether the runtime lives
//! behind the gateway WebSocket ([`WsDriver`]) or is embedded in this
//! process ([`LocalDriver`]). Embedded runs go through the same
//! `AgentHost::prepare_run` + `drive_run_stream` path as the gateway's WS
//! `agent` RPC and emit the exact same `agent` event frame JSON
//! (`{"type":"event","event":"agent","payload":...}`).

use crate::tui::AppState;
use crate::{CliError, GatewayClient};
use async_trait::async_trait;
use legion_core::config::Config;
use legion_host::AgentHost;
use legion_host::SessionStore;
use legion_protocol::WsFrame;
use legion_protocol::{AgentParams, UserMessage};
use legion_runtime::{
    ApprovalGate, ApprovalNotifier, ApprovalRequest, AskUserOutput, AskUserQuestion,
    NoOpApprovalNotifier, QuestionGate, QuestionNotifier, QuestionRequest,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Stderr notice printed whenever the CLI runs embedded instead of through
/// the gateway.
pub const EMBEDDED_NOTICE: &str = "gateway unreachable, running embedded (channels/cron inactive)";

/// Deadline for the Auto-mode gateway probe. Loopback connects succeed in
/// single-digit milliseconds; anything slower is treated as unreachable.
const GATEWAY_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// How the CLI reaches the agent runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    /// Probe the gateway briefly; fall back to the embedded runtime.
    Auto,
    /// Always run the runtime embedded in this process.
    Local,
    /// Always use the gateway (starting it if needed).
    Gateway,
}

/// Resolve the `--local` / `--gateway` flags into a [`CliMode`].
///
/// With no flags the default is **embedded** ([`CliMode::Local`]): a local
/// `legion` invocation runs the runtime in-process, so cwd, tools, and
/// approval all stay local with zero network hop. The gateway is a service
/// for remote channels (Telegram/Slack/cron) — connect to it explicitly with
/// `--gateway` when you want a shared long-running runtime. `--local` is a
/// no-op alias for the default (kept for clarity / scripting). `Auto`
/// (probe-and-fallback) is reachable programmatically but is no longer the
/// user-facing default.
pub fn resolve_cli_mode(_local: bool, gateway: bool) -> CliMode {
    if gateway {
        CliMode::Gateway
    } else {
        // `--local` and the default (no flag) both run embedded.
        CliMode::Local
    }
}

/// Probe the gateway with a short deadline (Auto mode). Returns the
/// connected client on success; any failure or timeout means "unreachable".
pub async fn probe_gateway(config: &Config) -> Option<GatewayClient> {
    tokio::time::timeout(GATEWAY_PROBE_TIMEOUT, GatewayClient::connect(config))
        .await
        .ok()
        .and_then(Result::ok)
}

/// One agent turn from the CLI's point of view.
#[async_trait]
pub trait TurnDriver: Send + Sync {
    /// Run one turn; events are injected into the TUI event channel as the
    /// exact same frame JSON the gateway WebSocket would deliver.
    async fn run_turn(&self, text: String) -> Result<(), CliError>;
    /// Fetch session history, shaped like the `sessions.history` RPC
    /// response (`ok` + `payload.messages`).
    async fn history(&self, session_key: &str) -> Result<Value, CliError>;
    /// Resolve a pending tool-approval prompt (the TUI's y/n answer to an
    /// `approval` event). Resolves against the gateway's shared registry in
    /// WebSocket mode and against the in-process gate in embedded mode.
    async fn resolve_approval(&self, prompt_id: &str, allow: bool);
    /// Resolve a pending `ask_user` question prompt. Resolves against the
    /// gateway's shared registry in WebSocket mode and against the in-process
    /// gate in embedded mode.
    async fn resolve_question(&self, prompt_id: &str, output: AskUserOutput);
    /// Schedule a recurring prompt as a cron job. Only the WebSocket driver
    /// can reach the gateway's cron scheduler; embedded mode returns an error.
    async fn schedule_loop(&self, cron: &str, prompt: &str) -> Result<String, CliError>;
    /// Short name shown in the TUI status bar: "gateway" | "local".
    fn mode_name(&self) -> &'static str;
    /// Start background plumbing after history has been fetched. Only the
    /// WebSocket driver needs this — its reader task would otherwise race
    /// the history request on the shared connection and consume the
    /// response frame. No-op for embedded drivers.
    fn start(&self, _state: Arc<Mutex<AppState>>, _event_tx: mpsc::UnboundedSender<Value>) {}
}

/// Drives turns over the gateway WebSocket (the classic path).
pub struct WsDriver {
    client: Arc<GatewayClient>,
    session_key: String,
    /// Yolo mode: forwarded to the gateway so its approval gate
    /// auto-approves every tool prompt for this client's runs.
    yolo: bool,
    /// Per-run workspace override forwarded to the gateway (cwd / --workspace).
    /// The gateway honors it for the "working" layer (tools/bootstrap/skills);
    /// tool path-validation still enforces boundaries.
    workspace_override: Option<PathBuf>,
}

impl WsDriver {
    pub fn new(
        client: Arc<GatewayClient>,
        session_key: String,
        yolo: bool,
        workspace_override: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            session_key,
            yolo,
            workspace_override,
        }
    }
}

#[async_trait]
impl TurnDriver for WsDriver {
    async fn run_turn(&self, text: String) -> Result<(), CliError> {
        let id = crate::tui::uuid_v4();
        let mut params = json!({
            "sessionKey": self.session_key,
            "message": { "role": "user", "content": text },
            "idempotencyKey": id,
            "wait": true,
            "yolo": self.yolo
        });
        if let Some(ws) = &self.workspace_override {
            params["workspace"] = json!(ws);
        }
        let req = json!({
            "type": "req",
            "id": id,
            "method": "agent",
            "params": params
        });
        self.client.send_json(&req).await
    }

    async fn history(&self, session_key: &str) -> Result<Value, CliError> {
        self.client
            .request("sessions.history", json!({ "sessionKey": session_key }))
            .await
    }

    async fn resolve_approval(&self, prompt_id: &str, allow: bool) {
        // Fire-and-forget: the gateway acks via a response frame that the
        // reader task surfaces like any other `res` frame.
        let req = json!({
            "type": "req",
            "id": crate::tui::uuid_v4(),
            "method": "approval.resolve",
            "params": { "promptId": prompt_id, "allow": allow }
        });
        if let Err(err) = self.client.send_json(&req).await {
            tracing::warn!(error = %err, "failed to send approval.resolve");
        }
    }

    async fn resolve_question(&self, prompt_id: &str, output: AskUserOutput) {
        let answers: HashMap<String, String> = output.answers;
        let questions: Vec<AskUserQuestion> = output.questions;
        let req = json!({
            "type": "req",
            "id": crate::tui::uuid_v4(),
            "method": "question.resolve",
            "params": {
                "promptId": prompt_id,
                "questions": questions,
                "answers": answers,
            }
        });
        if let Err(err) = self.client.send_json(&req).await {
            tracing::warn!(error = %err, "failed to send question.resolve");
        }
    }

    async fn schedule_loop(&self, cron: &str, prompt: &str) -> Result<String, CliError> {
        let resp = self
            .client
            .request(
                "cron.add",
                json!({
                    "schedule": cron,
                    "agent_id": crate::session_agent_id(&self.session_key).unwrap_or("main"),
                    "message": prompt,
                }),
            )
            .await?;
        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("cron.add failed");
            return Err(CliError::Gateway(err.to_string()));
        }
        let job_id = resp
            .get("payload")
            .and_then(|p| p.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(job_id)
    }

    fn mode_name(&self) -> &'static str {
        "gateway"
    }

    fn start(&self, state: Arc<Mutex<AppState>>, event_tx: mpsc::UnboundedSender<Value>) {
        // Forward WebSocket frames to the TUI loop until the connection
        // drops, then flip the status to "disconnected".
        let client = Arc::clone(&self.client);
        tokio::spawn(async move {
            while let Some(msg) = client.recv_json().await.ok().flatten() {
                let _ = event_tx.send(msg);
            }
            state.lock().unwrap().status = "disconnected".to_string();
            // Wake the UI loop so the status change gets drawn even though no
            // further websocket events will arrive (the loop only redraws on
            // events).
            let _ = event_tx.send(json!({ "type": "internal", "event": "disconnected" }));
        });
    }
}

/// Notifier that surfaces an embedded run's approval prompt to the TUI as
/// the exact same `approval` event frame the gateway WebSocket would emit,
/// so `handle_ws_event` handles both modes identically.
struct TuiApprovalNotifier {
    event_tx: mpsc::UnboundedSender<Value>,
}

#[async_trait]
impl ApprovalNotifier for TuiApprovalNotifier {
    async fn notify(&self, req: &ApprovalRequest, prompt_id: &str) {
        let _ = self.event_tx.send(json!({
            "type": "event",
            "event": "approval",
            "payload": {
                "promptId": prompt_id,
                "tool": req.tool,
                "agentId": req.agent_id,
                "sessionKey": req.session_key,
            }
        }));
    }
}

/// Notifier that surfaces an embedded run's `ask_user` prompt to the TUI as
/// the exact same `question` event frame the gateway WebSocket would emit.
struct TuiQuestionNotifier {
    event_tx: mpsc::UnboundedSender<Value>,
}

#[async_trait]
impl QuestionNotifier for TuiQuestionNotifier {
    async fn notify(&self, req: &QuestionRequest, prompt_id: &str, questions: &[AskUserQuestion]) {
        let _ = self.event_tx.send(json!({
            "type": "event",
            "event": "question",
            "payload": {
                "promptId": prompt_id,
                "tool": req.tool,
                "agentId": req.agent_id,
                "sessionKey": req.session_key,
                "questions": questions,
            }
        }));
    }
}

/// Drives turns against an [`AgentHost`] embedded in this process.
pub struct LocalDriver {
    host: Arc<AgentHost>,
    session_key: String,
    event_tx: mpsc::UnboundedSender<Value>,
    /// Yolo mode: this driver's per-turn approval gates auto-approve every
    /// tool prompt instead of surfacing it to the TUI.
    yolo: bool,
    /// Per-run workspace override (cwd / `--workspace`). Embedded-only; the
    /// gateway resolves workspace from its own config.
    workspace_override: Option<PathBuf>,
    /// The approval gate of the in-flight turn, if any. Each turn builds a
    /// fresh gate (mirroring the channel-side wiring), so resolves always
    /// target the latest turn's queue; a stale resolve is a harmless miss.
    current_gate: Mutex<Option<Arc<ApprovalGate>>>,
    /// The question gate of the in-flight turn, if any. Same lifetime as
    /// `current_gate`: a new turn replaces it and stale resolves are misses.
    current_question_gate: Mutex<Option<Arc<QuestionGate>>>,
}

impl LocalDriver {
    pub fn new(
        host: Arc<AgentHost>,
        session_key: String,
        event_tx: mpsc::UnboundedSender<Value>,
        yolo: bool,
        workspace_override: Option<PathBuf>,
    ) -> Self {
        Self {
            host,
            session_key,
            event_tx,
            yolo,
            workspace_override,
            current_gate: Mutex::new(None),
            current_question_gate: Mutex::new(None),
        }
    }
}

#[async_trait]
impl TurnDriver for LocalDriver {
    async fn run_turn(&self, text: String) -> Result<(), CliError> {
        // Build this turn's approval gate before preparing the run so an
        // early tool prompt cannot race ahead of the wiring.
        let gate = Arc::new(
            ApprovalGate::new(
                Arc::new(TuiApprovalNotifier {
                    event_tx: self.event_tx.clone(),
                }),
                Duration::from_secs(300),
            )
            .with_auto_approve(self.yolo),
        );
        *self.current_gate.lock().unwrap() = Some(gate.clone());
        // Build this turn's question gate so `ask_user` prompts reach the TUI.
        let question_gate = Arc::new(QuestionGate::new(
            Arc::new(TuiQuestionNotifier {
                event_tx: self.event_tx.clone(),
            }),
            Duration::from_secs(300),
        ));
        *self.current_question_gate.lock().unwrap() = Some(question_gate.clone());
        // Prepare the run before spawning so resolution/start failures
        // surface to the caller like a failed WS send would.
        let (stream, session_key, run_id, session_store) = prepare_local_run(
            &self.host,
            &self.session_key,
            text.clone(),
            false,
            self.yolo,
            Some(gate),
            Some(question_gate),
            self.workspace_override.clone(),
        )
        .await?;
        let event_tx = self.event_tx.clone();
        // Drive the run in the background so the TUI stays responsive;
        // events arrive on the same channel the WS reader would use.
        tokio::spawn(async move {
            legion_host::drive_run_stream(
                stream,
                session_store,
                session_key,
                text,
                run_id,
                move |frame| {
                    if let Ok(value) = serde_json::to_value(&frame) {
                        let _ = event_tx.send(value);
                    }
                },
            )
            .await;
        });
        Ok(())
    }

    async fn history(&self, session_key: &str) -> Result<Value, CliError> {
        let resolved = legion_host::routing::resolve_session_key(session_key, &self.host.router)
            .ok_or_else(|| CliError::Other(format!("invalid session key: {session_key}")))?;
        let mut messages = self.host.session_store.load_for_resume(&resolved).await;
        // Apply the same orphan repair as the WS handler so the TUI renders
        // exactly what the model will see.
        let _ = legion_host::recover_orphaned_tool_results(
            &mut messages,
            self.host.config.sessions.orphan_policy,
        );
        Ok(json!({
            "ok": true,
            "payload": {
                "sessionKey": resolved,
                "messages": serde_json::to_value(&messages)?,
            }
        }))
    }

    async fn resolve_approval(&self, prompt_id: &str, allow: bool) {
        let gate = self.current_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.resolve(prompt_id, allow).await;
        }
    }

    async fn resolve_question(&self, prompt_id: &str, output: AskUserOutput) {
        let gate = self.current_question_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.resolve(prompt_id, output).await;
        }
    }

    async fn schedule_loop(&self, _cron: &str, _prompt: &str) -> Result<String, CliError> {
        Err(CliError::Other(
            "/loop requires the gateway (embedded mode has no cron scheduler). Start the gateway with `legion gateway start`.".to_string(),
        ))
    }

    fn mode_name(&self) -> &'static str {
        "local"
    }
}

/// Build an embedded host from the loaded config, wrapping assembly errors.
pub async fn build_local_host(config: &Config) -> Result<AgentHost, CliError> {
    AgentHost::new(config.clone())
        .await
        .map_err(|err| CliError::Other(format!("embedded runtime error: {err}")))
}

/// Resolve the session key, load + repair history, and start an embedded
/// run. Returns the run stream plus everything `drive_run_stream` needs.
/// `approval_gate` is attached to the run when provided; without one the
/// runtime falls back to a no-op notifier and approval prompts time out.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_local_run(
    host: &AgentHost,
    session_key: &str,
    text: String,
    dump_prompts: bool,
    yolo: bool,
    approval_gate: Option<Arc<ApprovalGate>>,
    question_gate: Option<Arc<QuestionGate>>,
    workspace_override: Option<PathBuf>,
) -> Result<(legion_runtime::RunStream, String, String, Arc<SessionStore>), CliError> {
    let params = AgentParams {
        session_key: session_key.to_string(),
        message: UserMessage {
            role: "user".to_string(),
            content: text,
        },
        idempotency_key: Some(crate::tui::uuid_v4()),
        wait: true,
        history: Vec::new(),
        dump_prompts,
        yolo,
        workspace: workspace_override,
    };
    let (stream, accepted, resolved_key) = host
        .prepare_run(params, approval_gate, question_gate)
        .await
        .map_err(CliError::Other)?;
    Ok((
        stream,
        resolved_key,
        accepted.run_id,
        host.session_store.clone(),
    ))
}

/// Run one embedded turn to completion, forwarding each event frame to
/// `emit`. Used by `legion agent` (prints events); the TUI uses
/// [`LocalDriver`], which spawns the same drive loop in the background.
/// `legion agent` is non-interactive, so it passes no approval gate and
/// approval prompts fail closed after the runtime's timeout — unless `yolo`
/// is set, in which case an auto-approving gate is attached and every tool
/// prompt is accepted immediately.
pub async fn run_local_turn(
    host: &AgentHost,
    session_key: &str,
    text: String,
    dump_prompts: bool,
    yolo: bool,
    workspace_override: Option<PathBuf>,
    emit: impl FnMut(WsFrame),
) -> Result<(), CliError> {
    let approval_gate = yolo.then(|| {
        Arc::new(
            ApprovalGate::new(Arc::new(NoOpApprovalNotifier), Duration::from_secs(300))
                .with_auto_approve(true),
        )
    });
    let (stream, resolved_key, run_id, session_store) = prepare_local_run(
        host,
        session_key,
        text.clone(),
        dump_prompts,
        yolo,
        approval_gate,
        None,
        workspace_override,
    )
    .await?;
    legion_host::drive_run_stream(stream, session_store, resolved_key, text, run_id, emit).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    use std::ops::Range;
    use tempfile::TempDir;

    use legion_tools::ask_user::AskUserTool;

    // ---- mode resolution ----

    #[test]
    fn resolve_cli_mode_defaults_to_local() {
        // No flags = embedded (the new default). The CLI runs the runtime
        // in-process; the gateway is opt-in via --gateway.
        assert_eq!(resolve_cli_mode(false, false), CliMode::Local);
    }

    #[test]
    fn resolve_cli_mode_gateway_flag_wins_and_local_is_default() {
        // --local is an explicit no-op alias for the embedded default.
        assert_eq!(resolve_cli_mode(true, false), CliMode::Local);
        assert_eq!(resolve_cli_mode(false, true), CliMode::Gateway);
        // Clap makes the flags conflict; if both are set anyway, gateway wins
        // (explicit opt-in to the gateway beats the implicit local default).
        assert_eq!(resolve_cli_mode(true, true), CliMode::Gateway);
    }

    // ---- LocalDriver integration ----

    /// Provider that replies with all user messages it received, joined by
    /// commas, and captures every request so tests can verify that turn 2
    /// carried turn 1's history (mirrors ws_tests' HistoryEchoProvider).
    struct HistoryEchoProvider {
        requests: Arc<std::sync::Mutex<Vec<Vec<ChatMessage>>>>,
    }

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
            self.requests.lock().unwrap().push(req.messages);
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
        async fn search(
            &self,
            _query: &str,
            _top_k: usize,
        ) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(Vec::new())
        }

        async fn get(
            &self,
            _path: &str,
            _range: Option<Range<usize>>,
        ) -> Result<String, MemoryError> {
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

    fn history_runtime(
        config: Config,
        requests: Arc<std::sync::Mutex<Vec<Vec<ChatMessage>>>>,
    ) -> Arc<AgentRuntime> {
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(HistoryEchoProvider { requests }));
        Arc::new(AgentRuntime::new(
            Arc::new(router),
            Arc::new(FakeToolRegistry),
            Arc::new(FakeMemoryBackend),
            config,
        ))
    }

    fn is_lifecycle(frame: &Value, phase: &str) -> bool {
        frame["payload"]["stream"] == "lifecycle" && frame["payload"]["phase"] == phase
    }

    /// Drain event frames until the run's terminal lifecycle end arrives.
    async fn collect_until_end(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut frames = Vec::new();
        let drained = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(frame) = rx.recv().await {
                let is_end = is_lifecycle(&frame, "end");
                frames.push(frame);
                if is_end {
                    break;
                }
            }
        })
        .await;
        assert!(drained.is_ok(), "timed out waiting for lifecycle end");
        frames
    }

    #[tokio::test]
    async fn local_driver_streams_ws_shaped_frames_and_preserves_history() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let collection_path = tmp.path().join("memory");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let config = Config::from_json(&format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{ "defaults": {{ "workspace": "{}", "model": "history-echo/gpt-4o" }} }},
                "memory": {{
                    "builtin": {{
                        "collectionPath": "{}",
                        "embeddingDimension": 64
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/"),
            collection_path.display().to_string().replace('\\', "/"),
        ))
        .unwrap();

        let requests: Arc<std::sync::Mutex<Vec<Vec<ChatMessage>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut host = AgentHost::new(config.clone()).await.unwrap();
        // Override the assembled pieces with test doubles: a fake-provider
        // runtime and a transcript store rooted in the temp dir.
        host.runtime = history_runtime(config, requests.clone());
        host.session_store = Arc::new(SessionStore::new(tmp.path()));

        let session_key = "agent:main:dm:tui:default:direct:local-driver-test";
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let driver = LocalDriver::new(
            Arc::new(host),
            session_key.to_string(),
            event_tx,
            false,
            None,
        );
        assert_eq!(driver.mode_name(), "local");

        // ---- Round 1: frames arrive shaped exactly like the WS protocol.
        driver.run_turn("hello".to_string()).await.unwrap();
        let frames1 = collect_until_end(&mut event_rx).await;
        assert!(
            frames1.iter().any(|f| is_lifecycle(f, "start")),
            "expected lifecycle start frame"
        );
        assert!(
            frames1.iter().any(|f| {
                f["payload"]["stream"] == "assistant" && f["payload"]["delta"] == "hello"
            }),
            "expected assistant delta echoing the user message: {frames1:?}"
        );
        assert!(
            frames1.last().is_some_and(|f| is_lifecycle(f, "end")),
            "lifecycle end must be the last frame"
        );
        for frame in &frames1 {
            assert_eq!(frame["type"], "event");
            assert_eq!(frame["event"], "agent");
            assert!(
                frame["payload"]["run_id"].as_str().is_some(),
                "every payload carries run_id: {frame}"
            );
        }

        // Transcript landed on disk after the first turn.
        let transcript = tmp
            .path()
            .join("agents/main/sessions/local-driver-test.jsonl");
        assert!(transcript.exists(), "transcript not persisted");
        let on_disk = std::fs::read_to_string(&transcript).unwrap();
        assert!(on_disk.contains("hello"));

        // ---- Round 2: the model sees round 1 as history.
        driver.run_turn("again".to_string()).await.unwrap();
        let frames2 = collect_until_end(&mut event_rx).await;
        assert!(
            frames2.iter().any(|f| {
                f["payload"]["stream"] == "assistant" && f["payload"]["delta"] == "hello,again"
            }),
            "expected echo of both turns' user messages: {frames2:?}"
        );

        {
            let reqs = requests.lock().unwrap();
            assert_eq!(reqs.len(), 2, "one provider call per turn");
            let second = &reqs[1];
            assert!(
                second
                    .iter()
                    .any(|m| m.role == ChatRole::User && m.content == "hello"),
                "turn 2 request must include turn 1's user message"
            );
            assert!(
                second
                    .iter()
                    .any(|m| m.role == ChatRole::Assistant && m.content == "hello"),
                "turn 2 request must include turn 1's assistant reply"
            );
            assert!(
                second
                    .iter()
                    .any(|m| m.role == ChatRole::User && m.content == "again"),
                "turn 2 request must include the new user message"
            );
        }

        // ---- history(): same shape as the sessions.history RPC response.
        let resp = driver.history(session_key).await.unwrap();
        assert_eq!(resp["ok"], true);
        let messages = resp["payload"]["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            4,
            "two user messages and two assistant replies"
        );
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "hello");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["content"], "again");
        assert_eq!(messages[3]["content"], "hello,again");
    }

    // ---- LocalDriver approval loop ----

    /// Provider that emits an `exec` tool call on its first request and a
    /// final text answer once a tool result is in the conversation.
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

    /// An `exec` tool with `Approval::Required`, mirroring the real tool's
    /// default policy.
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

    #[tokio::test]
    async fn local_driver_surfaces_approval_and_resolves_in_process() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let collection_path = tmp.path().join("memory");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let config = Config::from_json(&format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{ "defaults": {{ "workspace": "{}", "model": "tool-call/gpt-4o" }} }},
                "memory": {{
                    "builtin": {{
                        "collectionPath": "{}",
                        "embeddingDimension": 64
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/"),
            collection_path.display().to_string().replace('\\', "/"),
        ))
        .unwrap();

        let mut host = AgentHost::new(config.clone()).await.unwrap();
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(ToolCallProvider));
        host.runtime = Arc::new(AgentRuntime::new(
            Arc::new(router),
            Arc::new(ExecToolRegistry),
            Arc::new(FakeMemoryBackend),
            config,
        ));
        host.session_store = Arc::new(SessionStore::new(tmp.path()));

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let driver = LocalDriver::new(
            Arc::new(host),
            "agent:main:dm:tui:default:direct:approval-test".to_string(),
            event_tx,
            false,
            None,
        );

        driver.run_turn("run ls".to_string()).await.unwrap();

        // The run must surface an `approval` event (not hang silently) before
        // the lifecycle end arrives.
        let prompt_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = event_rx.recv().await.expect("event channel closed");
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

        // Answering through the driver resolves the in-process gate and the
        // run continues to completion.
        driver.resolve_approval(&prompt_id, true).await;

        let frames = collect_until_end(&mut event_rx).await;
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "tool"
                    && f["payload"]["state"] == "end"
                    && f["payload"]["result"]["is_error"] == false
            }),
            "approved tool must execute successfully: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "assistant" && f["payload"]["delta"] == "done"
            }),
            "run must finish with the post-tool answer: {frames:?}"
        );
    }

    /// Provider that emits an `ask_user` tool call on its first request and a
    /// final text answer once a tool result is in the conversation.
    struct AskUserToolCallProvider;

    #[async_trait]
    impl Provider for AskUserToolCallProvider {
        fn id(&self) -> &str {
            "ask-user-tool-call"
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
                            name: "ask_user".into(),
                            arguments: serde_json::json!({
                                "questions": [{
                                    "question": "Which color?",
                                    "header": "Color",
                                    "options": [
                                        {"label": "Red", "description": "Warm"},
                                        {"label": "Blue", "description": "Cool"}
                                    ]
                                }]
                            })
                            .to_string(),
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

    struct AskUserToolRegistry;

    impl ToolRegistry for AskUserToolRegistry {
        fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
            if name == "ask_user" {
                Some(Arc::new(AskUserTool::new()))
            } else {
                None
            }
        }

        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "ask_user".to_string(),
                description: "ask_user".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }]
        }
    }

    #[tokio::test]
    async fn local_driver_surfaces_question_and_resolves_in_process() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let collection_path = tmp.path().join("memory");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let config = Config::from_json(&format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{ "defaults": {{ "workspace": "{}", "model": "ask-user-tool-call/gpt-4o" }} }},
                "memory": {{
                    "builtin": {{
                        "collectionPath": "{}",
                        "embeddingDimension": 64
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/"),
            collection_path.display().to_string().replace('\\', "/"),
        ))
        .unwrap();

        let mut host = AgentHost::new(config.clone()).await.unwrap();
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(AskUserToolCallProvider));
        host.runtime = Arc::new(AgentRuntime::new(
            Arc::new(router),
            Arc::new(AskUserToolRegistry),
            Arc::new(FakeMemoryBackend),
            config,
        ));
        host.session_store = Arc::new(SessionStore::new(tmp.path()));

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let driver = LocalDriver::new(
            Arc::new(host),
            "agent:main:dm:tui:default:direct:question-test".to_string(),
            event_tx,
            false,
            None,
        );

        driver.run_turn("ask me".to_string()).await.unwrap();

        let prompt_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = event_rx.recv().await.expect("event channel closed");
                if frame["type"] == "event" && frame["event"] == "question" {
                    assert_eq!(frame["payload"]["tool"], "ask_user");
                    break frame["payload"]["promptId"]
                        .as_str()
                        .expect("promptId")
                        .to_string();
                }
            }
        })
        .await
        .expect("timed out waiting for the question event");

        let output = legion_runtime::AskUserOutput {
            questions: vec![legion_runtime::AskUserQuestion {
                question: "Which color?".into(),
                header: "Color".into(),
                options: vec![
                    legion_runtime::AskUserOption {
                        label: "Red".into(),
                        description: "Warm".into(),
                        preview: None,
                    },
                    legion_runtime::AskUserOption {
                        label: "Blue".into(),
                        description: "Cool".into(),
                        preview: None,
                    },
                ],
                multi_select: false,
            }],
            answers: [("Which color?".into(), "Red".into())].into(),
            annotations: None,
        };
        driver.resolve_question(&prompt_id, output).await;

        let frames = collect_until_end(&mut event_rx).await;
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "tool"
                    && f["payload"]["state"] == "end"
                    && f["payload"]["result"]["is_error"] == false
            }),
            "answered ask_user must succeed: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "assistant" && f["payload"]["delta"] == "done"
            }),
            "run must finish with the post-tool answer: {frames:?}"
        );
    }

    /// `legion --yolo` (embedded TUI path): the per-turn gate auto-approves,
    /// so a `Required` tool runs without surfacing an approval event.
    #[tokio::test]
    async fn local_driver_yolo_skips_approval_prompt() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let collection_path = tmp.path().join("memory");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let config = Config::from_json(&format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{ "defaults": {{ "workspace": "{}", "model": "tool-call/gpt-4o" }} }},
                "memory": {{
                    "builtin": {{
                        "collectionPath": "{}",
                        "embeddingDimension": 64
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/"),
            collection_path.display().to_string().replace('\\', "/"),
        ))
        .unwrap();

        let mut host = AgentHost::new(config.clone()).await.unwrap();
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(ToolCallProvider));
        host.runtime = Arc::new(AgentRuntime::new(
            Arc::new(router),
            Arc::new(ExecToolRegistry),
            Arc::new(FakeMemoryBackend),
            config,
        ));
        host.session_store = Arc::new(SessionStore::new(tmp.path()));

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let driver = LocalDriver::new(
            Arc::new(host),
            "agent:main:dm:tui:default:direct:yolo-tui-test".to_string(),
            event_tx,
            true,
            None,
        );

        driver.run_turn("run ls".to_string()).await.unwrap();

        let frames = collect_until_end(&mut event_rx).await;
        assert!(
            frames.iter().all(|f| f["event"] != "approval"),
            "yolo mode must not surface approval prompts: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "tool"
                    && f["payload"]["state"] == "end"
                    && f["payload"]["result"]["is_error"] == false
            }),
            "yolo mode must auto-approve and execute the tool: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "assistant" && f["payload"]["delta"] == "done"
            }),
            "run must finish with the post-tool answer: {frames:?}"
        );
    }

    /// `legion agent --yolo` (embedded path): a `Required` tool runs without
    /// any approval prompt and without waiting on a human.
    #[tokio::test]
    async fn run_local_turn_yolo_auto_approves_required_tool() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let collection_path = tmp.path().join("memory");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let config = Config::from_json(&format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{ "defaults": {{ "workspace": "{}", "model": "tool-call/gpt-4o" }} }},
                "memory": {{
                    "builtin": {{
                        "collectionPath": "{}",
                        "embeddingDimension": 64
                    }}
                }}
            }}"#,
            workspace.display().to_string().replace('\\', "/"),
            collection_path.display().to_string().replace('\\', "/"),
        ))
        .unwrap();

        let mut host = AgentHost::new(config.clone()).await.unwrap();
        let mut router = ProviderRouter::new();
        router.register_provider(Arc::new(ToolCallProvider));
        host.runtime = Arc::new(AgentRuntime::new(
            Arc::new(router),
            Arc::new(ExecToolRegistry),
            Arc::new(FakeMemoryBackend),
            config,
        ));
        host.session_store = Arc::new(SessionStore::new(tmp.path()));

        let frames: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = frames.clone();
        run_local_turn(
            &host,
            "agent:main:dm:cli:default:direct:yolo-test",
            "run ls".to_string(),
            false,
            true,
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
                        .push(json!({ "event": event_type, "payload": payload }));
                }
            },
        )
        .await
        .unwrap();

        let frames = frames.lock().unwrap();
        assert!(
            frames.iter().all(|f| f["event"] != "approval"),
            "yolo mode must not surface approval prompts: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "tool"
                    && f["payload"]["state"] == "end"
                    && f["payload"]["result"]["is_error"] == false
            }),
            "yolo mode must auto-approve and execute the tool: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| {
                f["payload"]["stream"] == "assistant" && f["payload"]["delta"] == "done"
            }),
            "run must finish with the post-tool answer: {frames:?}"
        );
    }
}
