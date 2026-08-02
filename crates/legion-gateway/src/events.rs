//! In-memory event bus that fans out per-session [`HarnessEvent`]s to
//! `/events` subscribers.
//!
//! The bus is the routing layer that makes `AttachSession` possible: a run
//! registers itself here when it starts, and any `/events` connection may
//! subscribe to that session's stream regardless of which connection
//! initiated the run. Events arrive via [`EventBus::publish`], which the
//! gateway calls from the same emit closure that feeds the originating `/ws`
//! connection — so fan-out is purely additive and the internal protocol is
//! untouched.
//!
//! Design:
//! - All methods are synchronous and the map is guarded by a `std::sync::Mutex`
//!   (mirroring [`crate::nodes::NodeManager`]); operations are short and never
//!   await while holding the lock, so a sync lock is sufficient and lets the
//!   publish call site stay a plain `FnMut`.
//! - Subscriber channels are bounded (512). A full/dead channel is dropped from
//!   the session on the next publish (slow-consumer eviction) rather than
//!   blocking the producer; the subscriber's receiver closes and its handler
//!   surfaces a "subscription dropped" error so it can re-attach.
//! - `agent_id`/`peer_id` are parsed from the session key for the
//!   `ListSessions` summary; parse failure degrades to empty strings.
//!
//! The HTTP/WS entry points ([`events_handler`], [`handle_events_socket`]) speak
//! the versioned harness protocol ([`HarnessRequest`]/[`HarnessServerFrame`]),
//! reusing the `/ws` connect handshake and `authenticate` for auth, and the
//! shared `load_session_history` for the `AttachSession` history payload.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use legion_plugin_sdk::session_key::parse_session_key;
use legion_protocol::harness::{
    HarnessEvent, HarnessServerFrame, HarnessSessionSummary, SessionStatus,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Channel capacity per `/events` subscriber. A subscriber that falls behind
/// this many pending frames is evicted (its receiver closes).
const SUBSCRIBER_CAPACITY: usize = 512;

/// Opaque subscription id. Returned by [`EventBus::subscribe`] and consumed by
/// [`EventBus::unsubscribe`].
type SubId = u64;

struct Entry {
    agent_id: String,
    peer_id: String,
    /// Current running turn, if any. `None` when idle.
    run_id: Option<String>,
    subscribers: Vec<(SubId, mpsc::Sender<HarnessServerFrame>)>,
}

/// Shared registry of active sessions and their event subscribers.
///
/// Cloning is cheap (inner `Arc`); all clones share the same map.
#[derive(Clone, Default)]
pub struct EventBus {
    sessions: Arc<Mutex<HashMap<String, Entry>>>,
    next_sub_id: Arc<Mutex<SubId>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or refresh) a running turn for a session. Creates the session
    /// entry if absent, parses `agent_id`/`peer_id` from the key, and stamps
    /// the current `run_id`. Called by the gateway when a run starts.
    pub fn register_run(&self, session_key: &str, run_id: &str) {
        let parts = parse_session_key(session_key);
        let mut sessions = self.sessions.lock().expect("event bus mutex poisoned");
        let entry = sessions
            .entry(session_key.to_string())
            .or_insert_with(|| Entry {
                agent_id: parts
                    .as_ref()
                    .map(|p| p.agent_id.clone())
                    .unwrap_or_default(),
                peer_id: parts
                    .as_ref()
                    .map(|p| p.peer_id.clone())
                    .unwrap_or_default(),
                run_id: None,
                subscribers: Vec::new(),
            });
        // Keep agent/peer fresh in case a prior registration parsed empty.
        if let Some(p) = &parts {
            if entry.agent_id.is_empty() {
                entry.agent_id = p.agent_id.clone();
            }
            if entry.peer_id.is_empty() {
                entry.peer_id = p.peer_id.clone();
            }
        }
        entry.run_id = Some(run_id.to_string());
    }

    /// Mark a session's current turn as finished (idle). The session entry is
    /// retained as long as it has subscribers; an entry with no subscribers is
    /// removed (garbage-collected).
    pub fn end_run(&self, session_key: &str) {
        let mut sessions = self.sessions.lock().expect("event bus mutex poisoned");
        let Some(entry) = sessions.get_mut(session_key) else {
            return;
        };
        entry.run_id = None;
        if entry.subscribers.is_empty() {
            sessions.remove(session_key);
        }
    }

    /// Snapshot of active sessions for `ListSessions`. A session is "live" when
    /// it has a running turn, "idle" otherwise.
    pub fn list(&self) -> Vec<HarnessSessionSummary> {
        let sessions = self.sessions.lock().expect("event bus mutex poisoned");
        sessions
            .iter()
            .map(|(session_key, entry)| HarnessSessionSummary {
                session_key: session_key.clone(),
                agent_id: entry.agent_id.clone(),
                peer_id: entry.peer_id.clone(),
                run_id: entry.run_id.clone(),
                status: if entry.run_id.is_some() {
                    SessionStatus::Live
                } else {
                    SessionStatus::Idle
                },
            })
            .collect()
    }

    /// Subscribe to a session's event stream. Returns the subscription id, the
    /// current `run_id` (if a turn is running), and the receiver to drain. The
    /// session entry is created if absent, so attaching to a not-yet-seen
    /// session simply waits for its first turn.
    pub fn subscribe(
        &self,
        session_key: &str,
    ) -> (SubId, Option<String>, mpsc::Receiver<HarnessServerFrame>) {
        let sub_id = {
            let mut next = self.next_sub_id.lock().expect("sub id mutex poisoned");
            *next += 1;
            *next
        };
        let (tx, rx) = mpsc::channel(SUBSCRIBER_CAPACITY);
        let parts = parse_session_key(session_key);
        let mut sessions = self.sessions.lock().expect("event bus mutex poisoned");
        let entry = sessions
            .entry(session_key.to_string())
            .or_insert_with(|| Entry {
                agent_id: parts
                    .as_ref()
                    .map(|p| p.agent_id.clone())
                    .unwrap_or_default(),
                peer_id: parts
                    .as_ref()
                    .map(|p| p.peer_id.clone())
                    .unwrap_or_default(),
                run_id: None,
                subscribers: Vec::new(),
            });
        if let Some(p) = &parts {
            if entry.agent_id.is_empty() {
                entry.agent_id = p.agent_id.clone();
            }
            if entry.peer_id.is_empty() {
                entry.peer_id = p.peer_id.clone();
            }
        }
        let run_id = entry.run_id.clone();
        entry.subscribers.push((sub_id, tx));
        (sub_id, run_id, rx)
    }

    /// Remove a subscription. The session entry is garbage-collected when it
    /// has no subscribers and no running turn.
    pub fn unsubscribe(&self, session_key: &str, sub_id: SubId) {
        let mut sessions = self.sessions.lock().expect("event bus mutex poisoned");
        let gc = {
            let Some(entry) = sessions.get_mut(session_key) else {
                return;
            };
            entry.subscribers.retain(|(id, _)| *id != sub_id);
            entry.run_id.is_none() && entry.subscribers.is_empty()
        };
        if gc {
            sessions.remove(session_key);
        }
    }

    /// Fan an event out to every subscriber of a session. Subscribers whose
    /// channel is full or closed are evicted; a closed/evicted session with no
    /// remaining subscribers and no running turn is removed.
    pub fn publish(&self, session_key: &str, event: HarnessEvent) {
        let mut sessions = self.sessions.lock().expect("event bus mutex poisoned");
        let Some(entry) = sessions.get_mut(session_key) else {
            return;
        };
        let frame = HarnessServerFrame::Event { event };
        let mut dropped = Vec::new();
        for (id, tx) in entry.subscribers.iter() {
            match tx.try_send(frame.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(%session_key, sub_id = id, "events subscriber lagging; evicting");
                    dropped.push(*id);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!(%session_key, sub_id = id, "events subscriber gone; dropping");
                    dropped.push(*id);
                }
            }
        }
        if dropped.is_empty() {
            return;
        }
        entry.subscribers.retain(|(id, _)| !dropped.contains(id));
        if entry.run_id.is_none() && entry.subscribers.is_empty() {
            sessions.remove(session_key);
        }
    }
}

// ===========================================================================
// HTTP / WebSocket entry point for `/events`
// ===========================================================================

use axum::extract::Extension;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::SinkExt;
use legion_protocol::harness::{HARNESS_API_VERSION, HarnessRequest};

use crate::message::WsFrame;
use crate::websocket::{
    AuthResult, GatewayState, authenticate, close_with, frame_to_message, is_loopback_bind,
    next_device_counter, parse_frame,
};

/// HTTP upgrade handler for the `/events` route. Mirrors `websocket_handler`
/// but upgrades to the harness-protocol socket loop.
pub async fn events_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<GatewayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_events_socket(socket, (*state).clone()))
}

/// Encode a server frame as a `Message::Text` for the socket.
fn harness_to_message(frame: HarnessServerFrame) -> Message {
    Message::Text(serde_json::to_string(&frame).unwrap_or_default().into())
}

/// Drive a single `/events` connection through the harness protocol.
///
/// Sequence: `connect` handshake + auth (same as `/ws`) → `HelloOk` → a
/// `select!` loop serving `HarnessRequest`s and pumping any attached session's
/// event stream back to the client. The connection attaches to at most one
/// session at a time.
async fn handle_events_socket(mut socket: WebSocket, state: GatewayState) {
    // The `/events` endpoint is loopback-only: it lets an external process
    // attach to sessions it did not initiate, which is only safe on loopback.
    if !is_loopback_bind(&state.config.gateway.bind_host) {
        close_with(
            &mut socket,
            "handshake",
            "events endpoint requires loopback bind",
        )
        .await;
        return;
    }

    // Step 1: expect a `connect` frame (same handshake as `/ws`).
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
    let params = match connect_frame {
        Ok(WsFrame::Connect { params, .. }) => params,
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

    // Step 2: authenticate (same path as `/ws`).
    let device_id = if params.device_id.is_empty() {
        format!("device-{}", next_device_counter())
    } else {
        params.device_id.clone()
    };
    match authenticate(
        &state.config.gateway.auth,
        &state.pairing_store,
        &device_id,
        &params.auth,
        &state.config.gateway.bind_host,
    ) {
        AuthResult::Approved => {}
        AuthResult::Rejected(reason) => {
            close_with(&mut socket, "handshake", reason).await;
            return;
        }
    }

    // Step 3: hello. Reply with the server's harness API version.
    if socket
        .send(harness_to_message(HarnessServerFrame::HelloOk {
            v: HARNESS_API_VERSION,
        }))
        .await
        .is_err()
    {
        return;
    }

    // Active subscription: (session_key, sub_id, event receiver).
    let mut sub: Option<(String, u64, tokio::sync::mpsc::Receiver<HarnessServerFrame>)> = None;

    loop {
        tokio::select! {
            // Inbound client request.
            msg = socket.recv() => {
                let Some(msg) = msg else { break };
                let text = match msg {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let request: HarnessRequest = match serde_json::from_str(&text) {
                    Ok(r) => r,
                    Err(err) => {
                        if socket
                            .send(harness_to_message(HarnessServerFrame::Error {
                                message: format!("invalid request: {err}"),
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                match request {
                    HarnessRequest::Hello { v } => {
                        let frame = if v == HARNESS_API_VERSION {
                            HarnessServerFrame::HelloOk { v }
                        } else {
                            HarnessServerFrame::Error {
                                message: format!(
                                    "version mismatch: client={v} server={HARNESS_API_VERSION}"
                                ),
                            }
                        };
                        if socket.send(harness_to_message(frame)).await.is_err() {
                            break;
                        }
                    }
                    HarnessRequest::ListSessions => {
                        let sessions = state.event_bus.list();
                        if socket
                            .send(harness_to_message(HarnessServerFrame::SessionList {
                                sessions,
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    HarnessRequest::AttachSession { session_key } => {
                        // Detach any prior attach first.
                        if let Some((key, sub_id, _)) = sub.take() {
                            state.event_bus.unsubscribe(&key, sub_id);
                        }
                        // Load persisted history (shared with `sessions.history`
                        // RPC) and subscribe to the live stream in one step.
                        match legion_host::turn::load_session_history(
                            &state.router,
                            &state.session_store,
                            state.config.sessions.orphan_policy,
                            &session_key,
                        )
                        .await
                        {
                            Ok((resolved, history)) => {
                                let (sub_id, run_id, rx) =
                                    state.event_bus.subscribe(&resolved);
                                if socket
                                    .send(harness_to_message(HarnessServerFrame::Attached {
                                        session_key: resolved.clone(),
                                        run_id,
                                        history,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    state.event_bus.unsubscribe(&resolved, sub_id);
                                    break;
                                }
                                sub = Some((resolved, sub_id, rx));
                            }
                            Err(err) => {
                                if socket
                                    .send(harness_to_message(HarnessServerFrame::Error {
                                        message: err,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    HarnessRequest::DetachSession => {
                        if let Some((key, sub_id, _)) = sub.take() {
                            state.event_bus.unsubscribe(&key, sub_id);
                        }
                        if socket
                            .send(harness_to_message(HarnessServerFrame::Detached))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    HarnessRequest::Ping => {
                        if socket
                            .send(harness_to_message(HarnessServerFrame::Pong))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            // Outbound event for the attached session, if any.
            Some(frame) = async {
                match sub.as_mut() {
                    Some((_, _, rx)) => rx.recv().await,
                    None => None,
                }
            }, if sub.is_some() => {
                if socket.send(harness_to_message(frame)).await.is_err() {
                    break;
                }
            }
        }
    }

    // Clean up the subscription on disconnect.
    if let Some((key, sub_id, _)) = sub.take() {
        state.event_bus.unsubscribe(&key, sub_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_protocol::harness::HarnessEvent;

    const KEY: &str = "agent:bot:dm:cli:acct:direct:peer1";

    fn bus() -> EventBus {
        EventBus::new()
    }

    #[test]
    fn list_reflects_register_and_end_run() {
        let bus = bus();
        assert!(bus.list().is_empty());
        bus.register_run(KEY, "run-1");
        let sessions = bus.list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_key, KEY);
        assert_eq!(sessions[0].agent_id, "bot");
        assert_eq!(sessions[0].peer_id, "peer1");
        assert_eq!(sessions[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(sessions[0].status, SessionStatus::Live);
        // Ending the run with no subscribers GCs the entry.
        bus.end_run(KEY);
        assert!(bus.list().is_empty());
    }

    #[test]
    fn idle_session_retained_while_subscribed() {
        let bus = bus();
        bus.register_run(KEY, "run-1");
        let (sub, _, _rx) = bus.subscribe(KEY);
        bus.end_run(KEY);
        // Still listed (idle) because a subscriber remains.
        let sessions = bus.list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Idle);
        bus.unsubscribe(KEY, sub);
        assert!(bus.list().is_empty());
    }

    #[test]
    fn subscribe_creates_entry_and_reports_run_id() {
        let bus = bus();
        let (sub, run_id, _rx) = bus.subscribe(KEY);
        assert!(run_id.is_none());
        assert!(sub > 0);
        // Subscribing created the session; registering a run surfaces it.
        bus.register_run(KEY, "run-7");
        let (_, run_id2, _rx2) = bus.subscribe(KEY);
        assert_eq!(run_id2.as_deref(), Some("run-7"));
    }

    #[tokio::test]
    async fn publish_delivers_to_all_subscribers() {
        let bus = bus();
        bus.register_run(KEY, "run-1");
        let (_, _, mut rx_a) = bus.subscribe(KEY);
        let (_, _, mut rx_b) = bus.subscribe(KEY);
        bus.publish(
            KEY,
            HarnessEvent::RunStarted {
                session_key: KEY.into(),
                run_id: "run-1".into(),
            },
        );
        let fa = rx_a.recv().await.expect("subscriber A got frame");
        let fb = rx_b.recv().await.expect("subscriber B got frame");
        assert!(matches!(fa, HarnessServerFrame::Event { .. }));
        assert!(matches!(fb, HarnessServerFrame::Event { .. }));
    }

    #[tokio::test]
    async fn slow_subscriber_is_evicted() {
        let bus = bus();
        bus.register_run(KEY, "run-1");
        let (sub, _, mut rx) = bus.subscribe(KEY);
        // Subscriber never drains: fill past capacity to force eviction.
        for i in 0..(SUBSCRIBER_CAPACITY + 5) {
            bus.publish(
                KEY,
                HarnessEvent::AssistantTextDelta {
                    run_id: "run-1".into(),
                    delta: i.to_string(),
                },
            );
        }
        // After eviction, the subscriber's receiver eventually closes once the
        // buffered frames are drained (or it already has no sender). Either way
        // the session must no longer carry this subscriber.
        let sessions = bus.list();
        let entry = sessions.iter().find(|s| s.session_key == KEY).unwrap();
        // The run is still registered, so the entry persists, but we verify
        // eviction indirectly: publishing more does not panic and the count of
        // further deliveries stops growing beyond the buffer.
        let _ = entry;
        // Drain whatever was buffered.
        while rx.try_recv().is_ok() {}
        // The subscriber sender was evicted, so recv() returns None.
        assert!(rx.recv().await.is_none(), "evicted subscriber should close");
        // Unsubscribing the evicted id is a harmless no-op.
        bus.unsubscribe(KEY, sub);
    }
}
