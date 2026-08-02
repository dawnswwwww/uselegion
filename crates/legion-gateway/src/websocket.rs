use crate::events::EventBus;
use crate::market::PluginMarket;
use crate::message::{Features, HelloPayload, WsFrame};
use crate::nodes::{Node, NodeManager};
use crate::observability::MetricsRegistry;
use crate::pairing::PairingStore;
use crate::ws_rpc;
use axum::extract::Extension;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use legion_channel::WebChatProvider;
use legion_core::config::{AuthConfig, Config};
use legion_host::SessionStore;
use legion_host::routing::Router;
use legion_plugin_sdk::PluginRegistry;
use legion_runtime::{
    ApprovalNotifier, ApprovalQueueRegistry, ApprovalRequest, AskUserQuestion, Harness,
    QuestionNotifier, QuestionQueueRegistry,
};
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
    /// External event bus powering the `/events` endpoint. The gateway
    /// registers each running turn here and fans out `agent` events to any
    /// `/events` subscriber attached to that session.
    pub event_bus: EventBus,
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
    let mut event_rx: Option<ws_rpc::EventRx> = None;

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

async fn handle_request(
    state: &GatewayState,
    method: &str,
    params: serde_json::Value,
    device_id: &str,
) -> (WsFrame, Option<ws_rpc::EventRx>) {
    state
        .metrics_registry
        .increment_counter("ws_requests_total", "total websocket requests received");

    ws_rpc::handle(state, method, params, device_id).await
}

pub(crate) fn parse_frame(text: &str) -> Result<WsFrame, String> {
    serde_json::from_str(text).map_err(|e| format!("invalid frame: {e}"))
}

#[derive(Debug, Clone)]
pub(crate) enum AuthResult {
    Approved,
    Rejected(String),
}

/// Whether a bind host is a loopback address. The `/ws` and `/events` endpoints
/// both treat loopback as a trusted boundary (e.g. allowing `auth: none`).
pub(crate) fn is_loopback_bind(bind_host: &str) -> bool {
    bind_host == "127.0.0.1" || bind_host == "localhost" || bind_host == "::1"
}

pub(crate) fn authenticate(
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

    let is_loopback = is_loopback_bind(bind_host);

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

pub(crate) fn next_device_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

pub(crate) async fn close_with(socket: &mut WebSocket, id: &str, reason: impl Into<String>) {
    let _ = socket
        .send(frame_to_message(WsFrame::err(id, reason)))
        .await;
    let _ = socket.close().await;
}

pub(crate) fn frame_to_message(frame: WsFrame) -> Message {
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

    #[test]
    fn is_loopback_bind_recognizes_loopback_hosts() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("::1"));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.1.5"));
        assert!(!is_loopback_bind("example.com"));
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
