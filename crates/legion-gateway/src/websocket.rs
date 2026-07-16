use crate::market::PluginMarket;
use crate::message::{Features, HelloPayload, WsFrame};
use crate::nodes::{Node, NodeManager, is_allowed};
use crate::observability::MetricsRegistry;
use crate::pairing::PairingStore;
use axum::extract::Extension;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use legion_channel::WebChatProvider;
use legion_core::config::{AuthConfig, Config};
use legion_host::SessionStore;
use legion_host::routing::Router;
use legion_plugin_sdk::PluginRegistry;
use legion_plugin_sdk::channel::{OutboundMessage, Peer, PeerKind};
use legion_protocol::AgentParams;
use legion_runtime::{
    ApprovalGate, ApprovalNotifier, ApprovalQueueRegistry, ApprovalRequest, AskUserOutput,
    AskUserQuestion, Harness, QuestionGate, QuestionNotifier, QuestionQueueRegistry,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// Shared application state for the WebSocket handler.
#[derive(Clone)]
pub struct GatewayState {
    pub config: Config,
    pub pairing_store: PairingStore,
    pub runtime: Arc<dyn Harness>,
    pub router: Router,
    pub gateway_id: String,
    pub webchat: Arc<WebChatProvider>,
    pub registry: Arc<PluginRegistry>,
    pub cron_scheduler: Option<Arc<legion_automation::cron::CronScheduler>>,
    pub task_store: Option<legion_automation::tasks::SharedTaskStore>,
    pub task_runner: Option<Arc<legion_automation::task_runner::TaskRunner>>,
    pub node_manager: Arc<NodeManager>,
    pub metrics_registry: MetricsRegistry,
    pub plugin_market: PluginMarket,
    /// Persistent transcript store per session key.
    pub session_store: Arc<SessionStore>,
    /// Shared approval-queue registry: the runtime's per-run approval gates
    /// register pending prompts here so the `approval.resolve` RPC (and
    /// channel-side `approve:<id>` replies) can route decisions back.
    pub approval_registry: Arc<ApprovalQueueRegistry>,
    /// Shared question-queue registry: the runtime's per-run question gates
    /// register pending prompts here so the `question.resolve` RPC can route
    /// answers back.
    pub question_registry: Arc<QuestionQueueRegistry>,
}

/// Notifier that streams an `approval` event frame back to the WebSocket
/// client that started the run. The client answers with the
/// `approval.resolve` RPC, which resolves the prompt through the shared
/// [`ApprovalQueueRegistry`].
pub struct WsApprovalNotifier {
    tx: tokio::sync::mpsc::UnboundedSender<WsFrame>,
}

impl WsApprovalNotifier {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<WsFrame>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl ApprovalNotifier for WsApprovalNotifier {
    async fn notify(&self, req: &ApprovalRequest, prompt_id: &str) {
        let frame = WsFrame::event(
            "approval",
            json!({
                "promptId": prompt_id,
                "tool": req.tool,
                "agentId": req.agent_id,
                "sessionKey": req.session_key,
            }),
        );
        if self.tx.send(frame).is_err() {
            tracing::warn!(
                prompt_id,
                tool = %req.tool,
                "approval prompt undeliverable: connection closed"
            );
        }
    }
}

/// Notifier that streams a `question` event frame back to the WebSocket
/// client that started the run. The client answers with the
/// `question.resolve` RPC, which resolves the prompt through the shared
/// [`QuestionQueueRegistry`].
pub struct WsQuestionNotifier {
    tx: tokio::sync::mpsc::UnboundedSender<WsFrame>,
}

impl WsQuestionNotifier {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<WsFrame>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl QuestionNotifier for WsQuestionNotifier {
    async fn notify(
        &self,
        req: &legion_runtime::QuestionRequest,
        prompt_id: &str,
        questions: &[AskUserQuestion],
    ) {
        let frame = WsFrame::event(
            "question",
            json!({
                "promptId": prompt_id,
                "tool": req.tool,
                "agentId": req.agent_id,
                "sessionKey": req.session_key,
                "questions": questions,
            }),
        );
        if self.tx.send(frame).is_err() {
            tracing::warn!(
                prompt_id,
                tool = %req.tool,
                "question prompt undeliverable: connection closed"
            );
        }
    }
}

/// The HTTP upgrade handler attached to the Gateway router.
pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<GatewayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, (*state).clone()))
}

async fn handle_socket(mut socket: WebSocket, state: GatewayState) {
    // Step 1: expect a `connect` frame as the first message.
    let connect_frame = match socket.recv().await {
        Some(Ok(Message::Text(text))) => parse_frame(&text),
        Some(Ok(Message::Close(_))) | None => return,
        _ => {
            let _ = socket
                .send(frame_to_message(WsFrame::err(
                    "handshake",
                    "first frame must be text connect",
                )))
                .await;
            let _ = socket.close().await;
            return;
        }
    };

    let (conn_id, params) = match connect_frame {
        Ok(WsFrame::Connect { id, params }) => (id, params),
        Ok(_) => {
            close_with(
                &mut socket,
                "handshake",
                "first frame must be of type 'connect'",
            )
            .await;
            return;
        }
        Err(err) => {
            close_with(&mut socket, "handshake", err).await;
            return;
        }
    };

    // Step 2: authenticate.
    let creds = params.auth.clone();
    let device_id = if params.device_id.is_empty() {
        format!("device-{}", next_device_counter())
    } else {
        params.device_id.clone()
    };

    let approved = match authenticate(
        &state.config.gateway.auth,
        &state.pairing_store,
        &device_id,
        &creds,
        &state.config.gateway.bind_host,
    ) {
        AuthResult::Approved => true,
        AuthResult::Pending => {
            close_with(&mut socket, &conn_id, "pairing approval required").await;
            return;
        }
        AuthResult::Rejected(reason) => {
            close_with(&mut socket, &conn_id, &reason).await;
            return;
        }
    };

    if approved {
        state
            .pairing_store
            .auto_approve_loopback(crate::pairing::Device {
                device_id: device_id.clone(),
                platform: params.platform.clone(),
                device_family: params.device_family.clone(),
                role: params.role.clone(),
                approved: true,
                token: state.config.gateway.auth.token.clone().unwrap_or_default(),
                approved_at: None,
            });
    }

    state.metrics_registry.increment_counter(
        "ws_connections_total",
        "total websocket connections accepted",
    );

    // Step 3: send hello response.
    let hello = WsFrame::ok(
        &conn_id,
        HelloPayload {
            hello: "ok".to_string(),
            gateway_id: state.gateway_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: Some(legion_protocol::ProtocolCompatibility::current()),
            features: Features::default(),
            snapshot: json!({ "presence": {}, "health": {} }),
        },
    );

    if socket.send(frame_to_message(hello)).await.is_err() {
        return;
    }

    // Step 4: branch by connection role.
    if params.role == "node" {
        handle_node_socket(socket, state, device_id, params).await;
    } else {
        handle_client_socket(socket, state, device_id).await;
    }
}

async fn handle_client_socket(mut socket: WebSocket, state: GatewayState, device_id: String) {
    let mut seq: u64 = 0;
    let mut event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<WsFrame>> = None;

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => match parse_frame(&text) {
                        Ok(WsFrame::Req { id, method, params }) => {
                            let (response, new_rx) = handle_request(&state, &method, params, &device_id).await;
                            // Only replace the event stream when the request
                            // actually produces one — an unrelated RPC (e.g.
                            // `approval.resolve`) must not silently drop the
                            // in-flight agent run's frames.
                            if new_rx.is_some() {
                                event_rx = new_rx;
                            }
                            if socket
                                .send(frame_to_message(response.with_id(&id)))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(WsFrame::Event { event_type, payload, .. }) => {
                            // Client events are acknowledged as no-ops in MVP.
                            tracing::debug!(event = %event_type, payload = %payload, "client event received");
                        }
                        Ok(WsFrame::Connect { .. }) => {
                            let _ = socket
                                .send(frame_to_message(WsFrame::err(
                                    "protocol",
                                    "duplicate connect frame",
                                )))
                                .await;
                        }
                        Err(err) => {
                            let _ = socket
                                .send(frame_to_message(WsFrame::err("protocol", err)))
                                .await;
                        }
                        _ => {}
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            Some(frame) = async { event_rx.as_mut()?.recv().await }, if event_rx.is_some() => {
                if socket.send(frame_to_message(frame)).await.is_err() {
                    break;
                }
            }
        }

        seq += 1;

        // Periodically emit a tick event for demonstration.
        if seq % 60 == 0 {
            let tick = WsFrame::event("tick", json!({ "timestamp": seq }));
            let _ = socket.send(frame_to_message(tick)).await;
        }
    }

    // Emit shutdown event on close.
    let shutdown = WsFrame::event("shutdown", json!({ "device_id": device_id }));
    let _ = socket.send(frame_to_message(shutdown)).await;
}

async fn handle_node_socket(
    socket: WebSocket,
    state: GatewayState,
    device_id: String,
    params: crate::message::ConnectParams,
) {
    let display_name = if params.display_name.is_empty() {
        device_id.clone()
    } else {
        params.display_name.clone()
    };
    let node = Node::new(
        device_id.clone(),
        display_name,
        params.platform.clone(),
        params.device_family.clone(),
    )
    .with_commands(params.commands.clone())
    .with_capabilities(params.capabilities.clone())
    .with_permissions(params.permissions.clone().unwrap_or_default());

    let mut frame_rx = state.node_manager.connect(node);
    let (mut write, mut read) = socket.split();

    let node_id = device_id.clone();
    let manager = state.node_manager.clone();

    // Write task: forwards frames from the manager to the node.
    let write_handle = tokio::spawn(async move {
        while let Some(payload) = frame_rx.recv().await {
            let text = serde_json::to_string(&payload).unwrap_or_default();
            if write.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Read task: process incoming node events and acks.
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(payload) => {
                        let event_type = payload
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        if event_type == "node.invoke.res" {
                            if let (Some(correlation), Some(response)) = (
                                payload.get("correlation").and_then(|v| v.as_str()),
                                payload.get("response"),
                            ) {
                                manager.resolve(correlation, response.clone());
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "invalid node frame");
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => break,
            _ => {}
        }
    }

    write_handle.abort();
    manager.disconnect(&node_id);
}

type EventRx = tokio::sync::mpsc::UnboundedReceiver<WsFrame>;

#[derive(Debug, Clone, Deserialize)]
struct SendParams {
    pub channel: String,
    pub peer_id: String,
    pub text: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default, rename = "peerKind")]
    pub peer_kind: String,
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    #[serde(default, rename = "replyTo")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WebChatParams {
    pub peer_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MemorySearchParams {
    pub query: String,
    #[serde(default = "default_top_k")]
    #[allow(dead_code)]
    pub top_k: usize,
}

/// Parameters for the `sessions.history` method.
#[derive(Debug, Clone, Deserialize)]
struct SessionsHistoryParams {
    #[serde(rename = "sessionKey")]
    pub session_key: String,
}

/// Parameters for the `approval.resolve` method.
#[derive(Debug, Clone, Deserialize)]
struct ApprovalResolveParams {
    #[serde(rename = "promptId")]
    pub prompt_id: String,
    pub allow: bool,
}

/// Parameters for the `question.resolve` method.
#[derive(Debug, Clone, Deserialize)]
struct QuestionResolveParams {
    #[serde(rename = "promptId")]
    pub prompt_id: String,
    /// The questions that were originally asked (echoed back so the answer
    /// can be formatted with the same metadata).
    pub questions: Vec<AskUserQuestion>,
    /// Mapping from question text to the selected answer label. For
    /// multi-select questions the labels are comma-separated.
    pub answers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CronAddParams {
    pub schedule: String,
    #[serde(default = "default_agent_id")]
    pub agent_id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CronIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct FlowIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TaskIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TaskCreateParams {
    #[serde(default = "default_agent_id")]
    pub agent_id: String,
    pub message: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct NodeInvokeParams {
    pub node_id: String,
    pub command: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default = "default_node_invoke_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct NodeStatusParams {
    pub node_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MarketIdParams {
    pub id: String,
}

fn default_node_invoke_timeout_ms() -> u64 {
    30000
}

fn default_top_k() -> usize {
    5
}

fn default_agent_id() -> String {
    "main".to_string()
}

fn task_to_json(task: legion_automation::tasks::Task) -> serde_json::Value {
    json!({
        "id": task.id,
        "kind": task.kind,
        "status": task.status,
        "agent_id": task.agent_id,
        "session_id": task.session_id,
        "run_id": task.run_id,
        "created_at": task.created_at,
        "started_at": task.started_at,
        "ended_at": task.ended_at,
        "error": task.error,
        "message": task.message,
        "depends_on": task.depends_on,
    })
}

async fn handle_request(
    state: &GatewayState,
    method: &str,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    state
        .metrics_registry
        .increment_counter("ws_requests_total", "total websocket requests received");

    match method {
        "health" => (WsFrame::ok("", json!({ "status": "ok" })), None),
        "status" => (
            WsFrame::ok(
                "",
                json!({
                    "gateway_id": state.gateway_id,
                    "uptime_seconds": 0,
                    "channels": [],
                    "agents": state.config.agents.list.len() + 1,
                }),
            ),
            None,
        ),
        "send" => match serde_json::from_value::<SendParams>(params) {
            Ok(send_params) => {
                state
                    .metrics_registry
                    .increment_counter("channel_sends_total", "total outbound channel sends");
                let peer_kind = match send_params.peer_kind.as_str() {
                    "group" => PeerKind::Group,
                    "thread" => PeerKind::Thread,
                    _ => PeerKind::Direct,
                };
                let outbound = OutboundMessage {
                    channel: send_params.channel,
                    account_id: send_params.account_id.unwrap_or_else(|| "default".into()),
                    peer: Peer {
                        kind: peer_kind,
                        id: send_params.peer_id,
                        name: None,
                        thread_id: send_params.thread_id,
                    },
                    text: Some(send_params.text),
                    media: vec![],
                    reply_to: send_params.reply_to,
                };

                match state.registry.channel(&outbound.channel) {
                    Some(provider) => match provider.send(outbound).await {
                        Ok(()) => (WsFrame::ok("", json!({ "sent": true })), None),
                        Err(err) => (
                            WsFrame::err("", format!("channel send failed: {err}")),
                            None,
                        ),
                    },
                    None => (
                        WsFrame::err("", format!("channel '{}' not found", outbound.channel)),
                        None,
                    ),
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid send params: {err}")),
                None,
            ),
        },
        "webchat" => match serde_json::from_value::<WebChatParams>(params) {
            Ok(webchat_params) => {
                let inbound =
                    legion_channel::webchat_inbound(webchat_params.peer_id, webchat_params.text);
                match state.webchat.inject(inbound).await {
                    Ok(()) => (WsFrame::ok("", json!({ "injected": true })), None),
                    Err(err) => (
                        WsFrame::err("", format!("webchat inject failed: {err}")),
                        None,
                    ),
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid webchat params: {err}")),
                None,
            ),
        },
        "agent" => match serde_json::from_value::<AgentParams>(params) {
            Ok(agent_params) => {
                state
                    .metrics_registry
                    .increment_counter("agent_runs_total", "total agent runs started");

                let user_content = agent_params.message.content.clone();
                // The event channel is created before the run starts so the
                // approval notifier can stream `approval` events through the
                // same connection the `agent` events use; early frames simply
                // buffer until the caller installs the receiver.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsFrame>();
                // Interactive approval loop for Prompt/Required tools: the
                // notifier streams an `approval` event to this client, which
                // answers via the `approval.resolve` RPC routed through the
                // shared registry (same pattern as the channel-side
                // `approve:<id>` replies). Yolo mode short-circuits the gate
                // so every prompt is auto-approved without notifying.
                let approval_gate = Arc::new(
                    ApprovalGate::new(
                        Arc::new(WsApprovalNotifier::new(tx.clone())),
                        std::time::Duration::from_secs(300),
                    )
                    .with_registry(state.approval_registry.clone())
                    .with_auto_approve(agent_params.yolo),
                );
                // Interactive question loop for the `ask_user` tool: the
                // notifier streams a `question` event to this client, which
                // answers via the `question.resolve` RPC routed through the
                // shared question registry.
                let question_gate = Arc::new(
                    QuestionGate::new(
                        Arc::new(WsQuestionNotifier::new(tx.clone())),
                        std::time::Duration::from_secs(300),
                    )
                    .with_registry(state.question_registry.clone()),
                );
                // Resume prep (session-key resolution, transcript load +
                // orphan repair, run start) is shared with embedded hosts via
                // `legion_host::turn::prepare_run`; the state fields are passed
                // explicitly so `with_runtime`/`with_session_store` test
                // overrides keep applying.
                match legion_host::turn::prepare_run(
                    &*state.runtime,
                    &state.config,
                    &state.router,
                    &state.session_store,
                    agent_params,
                    Some(approval_gate),
                    Some(question_gate),
                )
                .await
                {
                    Ok((stream, accepted, session_key)) => {
                        let run_id = accepted.run_id.clone();
                        let session_store = state.session_store.clone();

                        tokio::spawn(async move {
                            legion_host::turn::drive_run_stream(
                                stream,
                                session_store,
                                session_key,
                                user_content,
                                run_id,
                                move |frame| {
                                    let _ = tx.send(frame);
                                },
                            )
                            .await;
                        });

                        (
                            WsFrame::ok(
                                "",
                                json!({
                                    "run_id": accepted.run_id,
                                    "accepted_at": accepted.accepted_at,
                                }),
                            ),
                            Some(rx),
                        )
                    }
                    Err(err) => (WsFrame::err("", err), None),
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid agent params: {err}")),
                None,
            ),
        },
        "approval.resolve" => match serde_json::from_value::<ApprovalResolveParams>(params) {
            Ok(resolve) => {
                let resolved = state
                    .approval_registry
                    .resolve(&resolve.prompt_id, resolve.allow)
                    .await;
                if resolved {
                    tracing::info!(
                        prompt_id = %resolve.prompt_id,
                        allow = resolve.allow,
                        "approval resolved via websocket"
                    );
                    (WsFrame::ok("", json!({ "resolved": true })), None)
                } else {
                    (
                        WsFrame::err(
                            "",
                            format!("no pending approval prompt '{}'", resolve.prompt_id),
                        ),
                        None,
                    )
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid approval.resolve params: {err}")),
                None,
            ),
        },
        "question.resolve" => match serde_json::from_value::<QuestionResolveParams>(params) {
            Ok(resolve) => {
                let output = AskUserOutput {
                    questions: resolve.questions,
                    answers: resolve.answers,
                    annotations: None,
                };
                let resolved = state
                    .question_registry
                    .resolve(&resolve.prompt_id, output)
                    .await;
                if resolved {
                    tracing::info!(
                        prompt_id = %resolve.prompt_id,
                        "question resolved via websocket"
                    );
                    (WsFrame::ok("", json!({ "resolved": true })), None)
                } else {
                    (
                        WsFrame::err(
                            "",
                            format!("no pending question prompt '{}'", resolve.prompt_id),
                        ),
                        None,
                    )
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid question.resolve params: {err}")),
                None,
            ),
        },
        "channels" => {
            let channels: Vec<serde_json::Value> = state
                .registry
                .channels()
                .iter()
                .map(|(id, provider)| {
                    json!({
                        "id": id,
                        "capabilities": provider.capabilities(),
                    })
                })
                .collect();
            (
                WsFrame::ok(
                    "",
                    json!({
                        "gateway_id": state.gateway_id,
                        "channels": channels,
                    }),
                ),
                None,
            )
        }
        "memory.search" => match serde_json::from_value::<MemorySearchParams>(params) {
            Ok(search) => {
                // MVP: return empty results. A real implementation would call
                // the configured memory backend.
                let results: Vec<serde_json::Value> = Vec::new();
                (
                    WsFrame::ok(
                        "",
                        json!({
                            "query": search.query,
                            "results": results,
                        }),
                    ),
                    None,
                )
            }
            Err(err) => (
                WsFrame::err("", format!("invalid memory.search params: {err}")),
                None,
            ),
        },
        "sessions.history" => match serde_json::from_value::<SessionsHistoryParams>(params) {
            Ok(history_params) => {
                match legion_host::routing::resolve_session_key(
                    &history_params.session_key,
                    &state.router,
                ) {
                    // Reject unsafe segments explicitly instead of silently
                    // answering with an empty history (`resolve_session_key`
                    // only checks the key shape; `SessionStore` would just
                    // resolve no path and return []).
                    Some(key)
                        if key
                            .rsplit(':')
                            .next()
                            .is_some_and(legion_host::session_tools::is_safe_peer_id) =>
                    {
                        let mut history = state.session_store.load_for_resume(&key).await;
                        // Apply the same orphan repair as the resume path so the
                        // client renders exactly what the model will see.
                        let _ = legion_host::recover_orphaned_tool_results(
                            &mut history,
                            state.config.sessions.orphan_policy,
                        );
                        tracing::info!(
                            session_key = %key,
                            messages = history.len(),
                            "served sessions.history"
                        );
                        (
                            WsFrame::ok(
                                "",
                                json!({
                                    "sessionKey": key,
                                    "messages": serde_json::to_value(&history)
                                        .unwrap_or_else(|_| json!([])),
                                }),
                            ),
                            None,
                        )
                    }
                    _ => (
                        WsFrame::err(
                            "",
                            format!("invalid session key: {}", history_params.session_key),
                        ),
                        None,
                    ),
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid sessions.history params: {err}")),
                None,
            ),
        },
        "system-presence" => (
            WsFrame::ok("", json!({ "device_id": _device_id, "status": "online" })),
            None,
        ),
        "cron.list" => {
            let jobs = match state.cron_scheduler.as_ref() {
                Some(scheduler) => match scheduler.list().await {
                    Ok(jobs) => jobs
                        .into_iter()
                        .map(|j| {
                            json!({
                                "id": j.id,
                                "agent_id": j.agent_id,
                                "message": j.message,
                                "schedule": j.schedule,
                                "enabled": j.enabled,
                                "next_run": j.next_run,
                                "last_run": j.last_run,
                            })
                        })
                        .collect::<Vec<_>>(),
                    Err(err) => {
                        return (WsFrame::err("", format!("cron list failed: {err}")), None);
                    }
                },
                None => Vec::new(),
            };
            (WsFrame::ok("", json!({ "jobs": jobs })), None)
        }
        "cron.add" => match serde_json::from_value::<CronAddParams>(params) {
            Ok(add) => {
                let at = match add.at {
                    Some(raw) => match legion_automation::cron::parse_at(&raw) {
                        Ok(dt) => Some(dt),
                        Err(err) => return (WsFrame::err("", err.to_string()), None),
                    },
                    None => None,
                };
                match state.cron_scheduler.as_ref() {
                    Some(scheduler) => match scheduler
                        .add(legion_automation::cron::AddJobRequest {
                            schedule: add.schedule,
                            agent_id: add.agent_id,
                            message: add.message,
                            at,
                            webhook_secret: add.webhook_secret,
                        })
                        .await
                    {
                        Ok(job) => (
                            WsFrame::ok(
                                "",
                                json!({
                                    "id": job.id,
                                    "schedule": job.schedule,
                                    "next_run": job.next_run,
                                }),
                            ),
                            None,
                        ),
                        Err(err) => (WsFrame::err("", err.to_string()), None),
                    },
                    None => (WsFrame::err("", "cron scheduler not available"), None),
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid cron.add params: {err}")),
                None,
            ),
        },
        "cron.remove" => match serde_json::from_value::<CronIdParams>(params) {
            Ok(remove) => match state.cron_scheduler.as_ref() {
                Some(scheduler) => match scheduler.remove(&remove.id).await {
                    Ok(()) => (WsFrame::ok("", json!({ "removed": true })), None),
                    Err(err) => (WsFrame::err("", err.to_string()), None),
                },
                None => (WsFrame::err("", "cron scheduler not available"), None),
            },
            Err(err) => (
                WsFrame::err("", format!("invalid cron.remove params: {err}")),
                None,
            ),
        },
        "cron.run" => match serde_json::from_value::<CronIdParams>(params) {
            Ok(run) => match state.cron_scheduler.as_ref() {
                Some(scheduler) => match scheduler.run(&run.id).await {
                    Ok(task) => (
                        WsFrame::ok(
                            "",
                            json!({
                                "task_id": task.id,
                                "status": task.status,
                            }),
                        ),
                        None,
                    ),
                    Err(err) => (WsFrame::err("", err.to_string()), None),
                },
                None => (WsFrame::err("", "cron scheduler not available"), None),
            },
            Err(err) => (
                WsFrame::err("", format!("invalid cron.run params: {err}")),
                None,
            ),
        },
        "tasks.list" => {
            let tasks = match state.task_store.as_ref() {
                Some(store) => match store.list().await {
                    Ok(tasks) => tasks.into_iter().map(task_to_json).collect::<Vec<_>>(),
                    Err(err) => {
                        return (WsFrame::err("", format!("tasks list failed: {err}")), None);
                    }
                },
                None => Vec::new(),
            };
            (WsFrame::ok("", json!({ "tasks": tasks })), None)
        }
        "tasks.show" => match serde_json::from_value::<TaskIdParams>(params) {
            Ok(show) => match state.task_store.as_ref() {
                Some(store) => match store.get(&show.id).await {
                    Ok(Some(task)) => (WsFrame::ok("", task_to_json(task)), None),
                    Ok(None) => (
                        WsFrame::err("", format!("task '{}' not found", show.id)),
                        None,
                    ),
                    Err(err) => (WsFrame::err("", err.to_string()), None),
                },
                None => (WsFrame::err("", "task store not available"), None),
            },
            Err(err) => (
                WsFrame::err("", format!("invalid tasks.show params: {err}")),
                None,
            ),
        },
        "tasks.create" => match serde_json::from_value::<TaskCreateParams>(params) {
            Ok(create) => match state.task_runner.as_ref() {
                Some(runner) => {
                    let req = legion_automation::task_runner::EnqueueRequest {
                        agent_id: create.agent_id,
                        message: create.message,
                        kind: legion_automation::tasks::TaskKind::Cli,
                        depends_on: create.depends_on,
                    };
                    match runner.enqueue(req).await {
                        Ok(task) => (WsFrame::ok("", task_to_json(task)), None),
                        Err(err) => (WsFrame::err("", err.to_string()), None),
                    }
                }
                None => (WsFrame::err("", "task runner not available"), None),
            },
            Err(err) => (
                WsFrame::err("", format!("invalid tasks.create params: {err}")),
                None,
            ),
        },
        "tasks.run" => match serde_json::from_value::<TaskIdParams>(params) {
            Ok(run) => match state.task_runner.as_ref() {
                Some(runner) => match runner.run(&run.id).await {
                    Ok(task) => (WsFrame::ok("", task_to_json(task)), None),
                    Err(err) => (WsFrame::err("", err.to_string()), None),
                },
                None => (WsFrame::err("", "task runner not available"), None),
            },
            Err(err) => (
                WsFrame::err("", format!("invalid tasks.run params: {err}")),
                None,
            ),
        },
        "flows.list" => (
            WsFrame::ok("", json!({ "flows": state.config.flows })),
            None,
        ),
        "flows.run" => match serde_json::from_value::<FlowIdParams>(params) {
            Ok(run) => {
                let flow = state.config.flows.iter().find(|f| f.id == run.id).cloned();
                match flow {
                    Some(flow) => {
                        let runner = legion_automation::flow::FlowRunner::new(
                            state.runtime.clone(),
                            state.config.clone(),
                        );
                        let report = runner.run_flow(&flow).await;
                        match serde_json::to_value(&report) {
                            Ok(value) => (WsFrame::ok("", value), None),
                            Err(err) => (
                                WsFrame::err(
                                    "",
                                    format!("flow report serialization failed: {err}"),
                                ),
                                None,
                            ),
                        }
                    }
                    None => (
                        WsFrame::err("", format!("flow '{}' not found", run.id)),
                        None,
                    ),
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid flows.run params: {err}")),
                None,
            ),
        },
        "nodes.list" => {
            let nodes: Vec<serde_json::Value> = state
                .node_manager
                .registry()
                .list()
                .into_iter()
                .map(|n| {
                    json!({
                        "id": n.id,
                        "display_name": n.display_name,
                        "platform": n.platform,
                        "device_family": n.device_family,
                        "commands": n.commands,
                        "capabilities": n.capabilities,
                        "permissions": n.permissions,
                        "paired": n.paired,
                    })
                })
                .collect();
            (WsFrame::ok("", json!({ "nodes": nodes })), None)
        }
        "nodes.status" => match serde_json::from_value::<NodeStatusParams>(params) {
            Ok(status) => match state.node_manager.registry().get(&status.node_id) {
                Some(n) => (
                    WsFrame::ok(
                        "",
                        json!({
                            "id": n.id,
                            "display_name": n.display_name,
                            "platform": n.platform,
                            "device_family": n.device_family,
                            "commands": n.commands,
                            "capabilities": n.capabilities,
                            "permissions": n.permissions,
                            "paired": n.paired,
                        }),
                    ),
                    None,
                ),
                None => (
                    WsFrame::err("", format!("node '{}' not found", status.node_id)),
                    None,
                ),
            },
            Err(err) => (
                WsFrame::err("", format!("invalid nodes.status params: {err}")),
                None,
            ),
        },
        "node.invoke" => match serde_json::from_value::<NodeInvokeParams>(params) {
            Ok(invoke) => {
                state
                    .metrics_registry
                    .increment_counter("node_invocations_total", "total node invocations");
                let platform = state
                    .node_manager
                    .registry()
                    .get(&invoke.node_id)
                    .map(|n| n.platform.clone())
                    .unwrap_or_default();
                if !is_allowed(&state.config.nodes, &platform, &invoke.command) {
                    return (
                        WsFrame::err(
                            "",
                            format!(
                                "command '{}' is not allowed for node '{}'",
                                invoke.command, invoke.node_id
                            ),
                        ),
                        None,
                    );
                }
                match state
                    .node_manager
                    .invoke(
                        &invoke.node_id,
                        &invoke.command,
                        invoke.params,
                        std::time::Duration::from_millis(invoke.timeout_ms),
                    )
                    .await
                {
                    Ok(response) => (WsFrame::ok("", response), None),
                    Err(err) => {
                        state.metrics_registry.increment_counter(
                            "node_invocations_failed_total",
                            "total failed node invocations",
                        );
                        (WsFrame::err("", err.to_string()), None)
                    }
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid node.invoke params: {err}")),
                None,
            ),
        },
        "market.list" => {
            let plugins: Vec<serde_json::Value> = state
                .plugin_market
                .list()
                .into_iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "version": p.version,
                        "kind": p.kind,
                        "description": p.description,
                        "installed": p.installed,
                    })
                })
                .collect();
            (WsFrame::ok("", json!({ "plugins": plugins })), None)
        }
        "market.install" => match serde_json::from_value::<MarketIdParams>(params) {
            Ok(install) => {
                if state.plugin_market.install(&install.id) {
                    (WsFrame::ok("", json!({ "installed": true })), None)
                } else {
                    (
                        WsFrame::err("", format!("plugin '{}' not found", install.id)),
                        None,
                    )
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid market.install params: {err}")),
                None,
            ),
        },
        "market.uninstall" => match serde_json::from_value::<MarketIdParams>(params) {
            Ok(uninstall) => {
                if state.plugin_market.uninstall(&uninstall.id) {
                    (WsFrame::ok("", json!({ "uninstalled": true })), None)
                } else {
                    (
                        WsFrame::err("", format!("plugin '{}' not found", uninstall.id)),
                        None,
                    )
                }
            }
            Err(err) => (
                WsFrame::err("", format!("invalid market.uninstall params: {err}")),
                None,
            ),
        },
        _ => (WsFrame::err("", format!("unknown method: {method}")), None),
    }
}

fn parse_frame(text: &str) -> Result<WsFrame, String> {
    serde_json::from_str(text).map_err(|e| format!("invalid frame: {e}"))
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum AuthResult {
    Approved,
    Pending,
    Rejected(String),
}

fn authenticate(
    auth: &AuthConfig,
    pairing: &PairingStore,
    device_id: &str,
    creds: &crate::message::AuthCreds,
    bind_host: &str,
) -> AuthResult {
    // Try device token first.
    if let Some(token) = &creds.token {
        if pairing.verify_token(device_id, token) {
            return AuthResult::Approved;
        }
    }

    let is_loopback = bind_host == "127.0.0.1" || bind_host == "localhost" || bind_host == "::1";

    match auth.mode.as_str() {
        "token" => {
            let expected = auth.token.clone().unwrap_or_default();
            if expected.is_empty() {
                // Loopback-only when no token configured.
                if is_loopback {
                    return AuthResult::Approved;
                }
                return AuthResult::Rejected("no token configured".to_string());
            }
            if creds.token.as_ref() == Some(&expected) {
                AuthResult::Approved
            } else {
                AuthResult::Rejected("invalid token".to_string())
            }
        }
        "password" => {
            let expected = auth.password.clone().unwrap_or_default();
            if creds.password.as_ref() == Some(&expected) && !expected.is_empty() {
                AuthResult::Approved
            } else {
                AuthResult::Rejected("invalid password".to_string())
            }
        }
        "trusted-proxy" => {
            // In trusted-proxy mode the proxy is responsible for auth;
            // we accept the connection and rely on pairing for device-level access.
            AuthResult::Approved
        }
        "none" => {
            if is_loopback {
                AuthResult::Approved
            } else {
                AuthResult::Rejected("auth mode 'none' only allowed on loopback".to_string())
            }
        }
        other => AuthResult::Rejected(format!("unsupported auth mode: {other}")),
    }
}

fn next_device_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

async fn close_with(socket: &mut WebSocket, id: &str, reason: impl Into<String>) {
    let _ = socket
        .send(frame_to_message(WsFrame::err(id, reason)))
        .await;
    let _ = socket.close().await;
}

fn frame_to_message(frame: WsFrame) -> Message {
    Message::Text(serde_json::to_string(&frame).unwrap_or_default().into())
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use crate::message::AuthCreds;

    fn auth_config(mode: &str, token: Option<&str>, password: Option<&str>) -> AuthConfig {
        AuthConfig {
            mode: mode.to_string(),
            token: token.map(str::to_string),
            password: password.map(str::to_string),
            allow_tailscale: false,
        }
    }

    fn creds(token: Option<&str>, password: Option<&str>) -> AuthCreds {
        AuthCreds {
            token: token.map(str::to_string),
            password: password.map(str::to_string),
        }
    }

    fn is_approved(result: &AuthResult) -> bool {
        matches!(result, AuthResult::Approved)
    }

    fn rejection(result: AuthResult) -> String {
        match result {
            AuthResult::Rejected(msg) => msg,
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn verified_device_token_short_circuits_auth_mode() {
        let pairing = PairingStore::new();
        let token = pairing.approve("dev-1");
        // Even an unknown auth mode cannot reject a verified device token.
        let auth = auth_config("carrier-pigeon", None, None);
        let result = authenticate(
            &auth,
            &pairing,
            "dev-1",
            &creds(Some(&token), None),
            "0.0.0.0",
        );
        assert!(is_approved(&result));
    }

    #[test]
    fn token_mode_without_configured_token_allows_loopback_only() {
        let pairing = PairingStore::new();
        let auth = auth_config("token", None, None);
        assert!(is_approved(&authenticate(
            &auth,
            &pairing,
            "dev",
            &creds(None, None),
            "127.0.0.1"
        )));
        let rejected = authenticate(&auth, &pairing, "dev", &creds(None, None), "0.0.0.0");
        assert_eq!(rejection(rejected), "no token configured");
    }

    #[test]
    fn token_mode_with_configured_token_requires_exact_match() {
        let pairing = PairingStore::new();
        let auth = auth_config("token", Some("secret"), None);
        let wrong = authenticate(
            &auth,
            &pairing,
            "dev",
            &creds(Some("wrong"), None),
            "127.0.0.1",
        );
        assert_eq!(rejection(wrong), "invalid token");
        assert!(is_approved(&authenticate(
            &auth,
            &pairing,
            "dev",
            &creds(Some("secret"), None),
            "127.0.0.1"
        )));
    }

    #[test]
    fn password_mode_never_approves_empty_password() {
        let pairing = PairingStore::new();
        let auth = auth_config("password", None, None);
        // Empty configured password + empty presented password must still be
        // rejected (the `!expected.is_empty()` guard).
        let result = authenticate(&auth, &pairing, "dev", &creds(None, Some("")), "127.0.0.1");
        assert_eq!(rejection(result), "invalid password");
    }

    #[test]
    fn password_mode_accepts_correct_password() {
        let pairing = PairingStore::new();
        let auth = auth_config("password", None, Some("hunter2"));
        assert!(is_approved(&authenticate(
            &auth,
            &pairing,
            "dev",
            &creds(None, Some("hunter2")),
            "127.0.0.1"
        )));
        let wrong = authenticate(
            &auth,
            &pairing,
            "dev",
            &creds(None, Some("wrong")),
            "127.0.0.1",
        );
        assert_eq!(rejection(wrong), "invalid password");
    }

    #[test]
    fn trusted_proxy_mode_approves() {
        let pairing = PairingStore::new();
        let auth = auth_config("trusted-proxy", None, None);
        assert!(is_approved(&authenticate(
            &auth,
            &pairing,
            "dev",
            &creds(None, None),
            "0.0.0.0"
        )));
    }

    #[test]
    fn none_mode_allows_loopback_only() {
        let pairing = PairingStore::new();
        let auth = auth_config("none", None, None);
        assert!(is_approved(&authenticate(
            &auth,
            &pairing,
            "dev",
            &creds(None, None),
            "127.0.0.1"
        )));
        let rejected = authenticate(&auth, &pairing, "dev", &creds(None, None), "0.0.0.0");
        assert_eq!(
            rejection(rejected),
            "auth mode 'none' only allowed on loopback"
        );
    }

    #[test]
    fn unknown_mode_is_rejected_with_mode_name() {
        let pairing = PairingStore::new();
        let auth = auth_config("carrier-pigeon", None, None);
        let rejected = authenticate(&auth, &pairing, "dev", &creds(None, None), "127.0.0.1");
        assert_eq!(rejection(rejected), "unsupported auth mode: carrier-pigeon");
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;

    #[tokio::test]
    async fn ws_question_notifier_streams_question_event_frame() {
        use legion_runtime::{AskUserOption, AskUserQuestion, QuestionRequest};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WsFrame>();
        let notifier = WsQuestionNotifier::new(tx);
        let req = QuestionRequest {
            tool: "ask_user".into(),
            agent_id: "main".into(),
            session_key: "agent:main:dm:tui:default:direct:p1".into(),
            interactive: true,
        };
        let questions = vec![AskUserQuestion {
            question: "Which color?".into(),
            header: "Color".into(),
            options: vec![
                AskUserOption {
                    label: "Red".into(),
                    description: "Warm".into(),
                    preview: None,
                },
                AskUserOption {
                    label: "Blue".into(),
                    description: "Cool".into(),
                    preview: None,
                },
            ],
            multi_select: false,
        }];

        notifier.notify(&req, "question-0", &questions).await;

        let frame = rx.try_recv().expect("notifier must emit a frame");
        match frame {
            WsFrame::Event {
                event_type,
                payload,
                ..
            } => {
                assert_eq!(event_type, "question");
                assert_eq!(payload["promptId"], "question-0");
                assert_eq!(payload["tool"], "ask_user");
                assert_eq!(payload["agentId"], "main");
                assert_eq!(payload["questions"].as_array().unwrap().len(), 1);
            }
            other => panic!("expected an event frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ws_question_notifier_tolerates_closed_connection() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsFrame>();
        drop(rx);
        let notifier = WsQuestionNotifier::new(tx);
        let req = legion_runtime::QuestionRequest {
            tool: "ask_user".into(),
            agent_id: "main".into(),
            session_key: "agent:main:dm:tui:default:direct:p1".into(),
            interactive: true,
        };
        notifier.notify(&req, "question-1", &[]).await;
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[tokio::test]
    async fn ws_approval_notifier_streams_approval_event_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WsFrame>();
        let notifier = WsApprovalNotifier::new(tx);
        let req = ApprovalRequest {
            tool: "exec".into(),
            agent_id: "main".into(),
            session_key: "agent:main:dm:tui:default:direct:p1".into(),
            interactive: true,
        };

        notifier.notify(&req, "prompt-0").await;

        let frame = rx.try_recv().expect("notifier must emit a frame");
        match frame {
            WsFrame::Event {
                event_type,
                payload,
                ..
            } => {
                assert_eq!(event_type, "approval");
                assert_eq!(payload["promptId"], "prompt-0");
                assert_eq!(payload["tool"], "exec");
                assert_eq!(payload["agentId"], "main");
                assert_eq!(payload["sessionKey"], "agent:main:dm:tui:default:direct:p1");
            }
            other => panic!("expected an event frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ws_approval_notifier_tolerates_closed_connection() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsFrame>();
        drop(rx);
        let notifier = WsApprovalNotifier::new(tx);
        let req = ApprovalRequest {
            tool: "exec".into(),
            agent_id: "main".into(),
            session_key: "agent:main:dm:tui:default:direct:p1".into(),
            interactive: true,
        };
        // Must not panic when the client is gone; the prompt just times out.
        notifier.notify(&req, "prompt-1").await;
    }
}
