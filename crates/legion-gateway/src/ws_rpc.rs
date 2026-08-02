use crate::message::WsFrame;
use crate::nodes::is_allowed;
use crate::websocket::{GatewayState, WsApprovalNotifier, WsQuestionNotifier};
use legion_plugin_sdk::channel::{OutboundMessage, Peer, PeerKind};
use legion_protocol::AgentParams;
use legion_protocol::harness::HarnessEvent;
use legion_runtime::{
    ApprovalGate, ApprovalNotifier, AskUserOutput, AskUserQuestion, QuestionGate, QuestionNotifier,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(crate) type EventRx = tokio::sync::mpsc::UnboundedReceiver<WsFrame>;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SendParams {
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
pub(crate) struct WebChatParams {
    pub peer_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MemorySearchParams {
    pub query: String,
}

/// Parameters for the `sessions.history` method.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SessionsHistoryParams {
    #[serde(rename = "sessionKey")]
    pub session_key: String,
}

/// Parameters for the `approval.resolve` method.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApprovalResolveParams {
    #[serde(rename = "promptId")]
    pub prompt_id: String,
    pub allow: bool,
}

/// Parameters for the `question.resolve` method.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct QuestionResolveParams {
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
pub(crate) struct CronAddParams {
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
pub(crate) struct CronIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FlowIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TaskIdParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TaskCreateParams {
    #[serde(default = "default_agent_id")]
    pub agent_id: String,
    pub message: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NodeInvokeParams {
    pub node_id: String,
    pub command: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default = "default_node_invoke_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NodeStatusParams {
    pub node_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MarketIdParams {
    pub id: String,
}

pub(crate) fn default_node_invoke_timeout_ms() -> u64 {
    30000
}

pub(crate) fn default_agent_id() -> String {
    "main".to_string()
}

pub(crate) fn task_to_json(task: legion_automation::tasks::Task) -> serde_json::Value {
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

pub(crate) fn cron_job_to_json(j: legion_automation::cron::CronJob) -> serde_json::Value {
    json!({
        "id": j.id,
        "agent_id": j.agent_id,
        "message": j.message,
        "schedule": j.schedule,
        "enabled": j.enabled,
        "next_run": j.next_run,
        "last_run": j.last_run,
    })
}

pub(crate) async fn handle(
    state: &GatewayState,
    method: &str,
    params: serde_json::Value,
    device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match method {
        "health" => handle_health(state, params, device_id).await,
        "status" => handle_status(state, params, device_id).await,
        "send" => handle_send(state, params, device_id).await,
        "webchat" => handle_webchat(state, params, device_id).await,
        "agent" => handle_agent(state, params, device_id).await,
        "approval.resolve" => handle_approval_resolve(state, params, device_id).await,
        "question.resolve" => handle_question_resolve(state, params, device_id).await,
        "channels" => handle_channels(state, params, device_id).await,
        "memory.search" => handle_memory_search(state, params, device_id).await,
        "sessions.history" => handle_sessions_history(state, params, device_id).await,
        "system-presence" => handle_system_presence(state, params, device_id).await,
        "cron.list" => handle_cron_list(state, params, device_id).await,
        "cron.add" => handle_cron_add(state, params, device_id).await,
        "cron.remove" => handle_cron_remove(state, params, device_id).await,
        "cron.run" => handle_cron_run(state, params, device_id).await,
        "tasks.list" => handle_tasks_list(state, params, device_id).await,
        "tasks.show" => handle_tasks_show(state, params, device_id).await,
        "tasks.create" => handle_tasks_create(state, params, device_id).await,
        "tasks.run" => handle_tasks_run(state, params, device_id).await,
        "flows.list" => handle_flows_list(state, params, device_id).await,
        "flows.run" => handle_flows_run(state, params, device_id).await,
        "nodes.list" => handle_nodes_list(state, params, device_id).await,
        "nodes.status" => handle_nodes_status(state, params, device_id).await,
        "node.invoke" => handle_node_invoke(state, params, device_id).await,
        "market.list" => handle_market_list(state, params, device_id).await,
        "market.install" => handle_market_install(state, params, device_id).await,
        "market.uninstall" => handle_market_uninstall(state, params, device_id).await,
        _ => (WsFrame::err("", format!("unknown method: {method}")), None),
    }
}

pub(crate) async fn handle_health(
    _state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    (WsFrame::ok("", json!({ "status": "ok" })), None)
}

pub(crate) async fn handle_status(
    state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    (
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
    )
}

pub(crate) async fn handle_send(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<SendParams>(params) {
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
    }
}

pub(crate) async fn handle_webchat(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<WebChatParams>(params) {
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
    }
}

pub(crate) async fn handle_agent(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<AgentParams>(params) {
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
                    Arc::new(WsApprovalNotifier::new(tx.clone())) as Arc<dyn ApprovalNotifier>,
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
                    Arc::new(WsQuestionNotifier::new(tx.clone())) as Arc<dyn QuestionNotifier>,
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

                    // Register the run with the external event bus so `/events`
                    // subscribers attached to this session receive its events.
                    // The emit closure below mirrors each `agent` frame into the
                    // bus alongside the existing forward to the originating
                    // `/ws` connection — the internal protocol is untouched.
                    let bus = state.event_bus.clone();
                    let key = session_key.clone();
                    bus.register_run(&session_key, &run_id);

                    tokio::spawn(async move {
                        let end_bus = bus.clone();
                        if let Err(err) = legion_host::turn::drive_run_stream(
                            stream,
                            session_store,
                            session_key.clone(),
                            user_content,
                            run_id.clone(),
                            move |frame| {
                                let _ = tx.send(frame.clone());
                                if let WsFrame::Event {
                                    event_type,
                                    payload,
                                    ..
                                } = &frame
                                {
                                    if event_type == "agent" {
                                        if let Some(ev) =
                                            HarnessEvent::from_agent_payload(&key, payload)
                                        {
                                            bus.publish(&key, ev);
                                        }
                                    }
                                }
                            },
                        )
                        .await
                        {
                            tracing::error!(error = %err, "failed to persist session transcript");
                        }
                        end_bus.end_run(&session_key);
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
    }
}

pub(crate) async fn handle_approval_resolve(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<ApprovalResolveParams>(params) {
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
    }
}

pub(crate) async fn handle_question_resolve(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<QuestionResolveParams>(params) {
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
    }
}

pub(crate) async fn handle_channels(
    state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
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

pub(crate) async fn handle_memory_search(
    _state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<MemorySearchParams>(params) {
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
    }
}

pub(crate) async fn handle_sessions_history(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<SessionsHistoryParams>(params) {
        Ok(history_params) => {
            // History loading (resolve, orphan repair) is shared with the
            // embedded CLI host via `legion_host::turn::load_session_history`
            // so the client renders exactly what the model will see.
            match legion_host::turn::load_session_history(
                &state.router,
                &state.session_store,
                state.config.sessions.orphan_policy,
                &history_params.session_key,
            )
            .await
            {
                Ok((key, history)) => {
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
                Err(err) => (WsFrame::err("", err), None),
            }
        }
        Err(err) => (
            WsFrame::err("", format!("invalid sessions.history params: {err}")),
            None,
        ),
    }
}

pub(crate) async fn handle_system_presence(
    _state: &GatewayState,
    _params: serde_json::Value,
    device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    (
        WsFrame::ok("", json!({ "device_id": device_id, "status": "online" })),
        None,
    )
}

pub(crate) async fn handle_cron_list(
    state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    let jobs = match state.cron_scheduler.as_ref() {
        Some(scheduler) => match scheduler.list().await {
            Ok(jobs) => jobs.into_iter().map(cron_job_to_json).collect::<Vec<_>>(),
            Err(err) => {
                return (WsFrame::err("", format!("cron list failed: {err}")), None);
            }
        },
        None => Vec::new(),
    };
    (WsFrame::ok("", json!({ "jobs": jobs })), None)
}

pub(crate) async fn handle_cron_add(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<CronAddParams>(params) {
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
                        name: String::new(),
                        enabled: true,
                        webhook_secret: add.webhook_secret,
                        id_prefix: None,
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
    }
}

pub(crate) async fn handle_cron_remove(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<CronIdParams>(params) {
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
    }
}

pub(crate) async fn handle_cron_run(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<CronIdParams>(params) {
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
    }
}

pub(crate) async fn handle_tasks_list(
    state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
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

pub(crate) async fn handle_tasks_show(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<TaskIdParams>(params) {
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
    }
}

pub(crate) async fn handle_tasks_create(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<TaskCreateParams>(params) {
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
    }
}

pub(crate) async fn handle_tasks_run(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<TaskIdParams>(params) {
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
    }
}

pub(crate) async fn handle_flows_list(
    state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    (
        WsFrame::ok("", json!({ "flows": state.config.flows })),
        None,
    )
}

pub(crate) async fn handle_flows_run(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<FlowIdParams>(params) {
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
                            WsFrame::err("", format!("flow report serialization failed: {err}")),
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
    }
}

pub(crate) async fn handle_nodes_list(
    state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
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

pub(crate) async fn handle_nodes_status(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<NodeStatusParams>(params) {
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
    }
}

pub(crate) async fn handle_node_invoke(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<NodeInvokeParams>(params) {
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
    }
}

pub(crate) async fn handle_market_list(
    state: &GatewayState,
    _params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
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

pub(crate) async fn handle_market_install(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<MarketIdParams>(params) {
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
    }
}

pub(crate) async fn handle_market_uninstall(
    state: &GatewayState,
    params: serde_json::Value,
    _device_id: &str,
) -> (WsFrame, Option<EventRx>) {
    match serde_json::from_value::<MarketIdParams>(params) {
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
    }
}

#[cfg(test)]
mod tests {
    /// Methods dispatched by `handle` above. Keep this list next to the match
    /// so both are updated together; the test below pins it against the
    /// advertised `Features::default()` from `legion-protocol`.
    const DISPATCHED_METHODS: &[&str] = &[
        "health",
        "status",
        "send",
        "webchat",
        "agent",
        "approval.resolve",
        "question.resolve",
        "channels",
        "memory.search",
        "sessions.history",
        "system-presence",
        "cron.list",
        "cron.add",
        "cron.remove",
        "cron.run",
        "tasks.list",
        "tasks.show",
        "tasks.create",
        "tasks.run",
        "flows.list",
        "flows.run",
        "nodes.list",
        "nodes.status",
        "node.invoke",
        "market.list",
        "market.install",
        "market.uninstall",
    ];

    #[test]
    fn features_match_dispatch_table() {
        let advertised = legion_protocol::Features::default().methods;
        for method in DISPATCHED_METHODS {
            assert!(
                advertised.iter().any(|m| m == method),
                "Features::default() is missing dispatched method '{method}'"
            );
        }
        for method in &advertised {
            assert!(
                DISPATCHED_METHODS.contains(&method.as_str()),
                "Features::default() advertises '{method}' but ws_rpc does not dispatch it"
            );
        }
    }
}
