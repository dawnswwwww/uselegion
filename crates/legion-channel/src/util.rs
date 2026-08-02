//! Shared helpers for the built-in channel providers: config-key fallback,
//! HTTP/JSON boilerplate, the provider lifecycle skeleton, and the fixed
//! reconnect delay.

use legion_plugin_sdk::channel::ChannelError;
use serde::Serialize;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Config lookup
// ---------------------------------------------------------------------------

/// Look up a string config value, trying each key in order (callers pass the
/// camelCase key first, then its snake_case alias). A present-but-non-string
/// value shadows later keys, matching the previous per-provider behavior.
pub(crate) fn cfg_str<'a>(config: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| config.get(*key))
        .and_then(|v| v.as_str())
}

/// Required string config value; `missing` is the `InvalidConfig` message.
pub(crate) fn cfg_required(
    config: &Value,
    keys: &[&str],
    missing: &str,
) -> Result<String, ChannelError> {
    cfg_str(config, keys)
        .ok_or_else(|| ChannelError::InvalidConfig(missing.into()))
        .map(str::to_string)
}

/// String config value with a fallback default.
pub(crate) fn cfg_str_or(config: &Value, keys: &[&str], default: &str) -> String {
    cfg_str(config, keys).unwrap_or(default).to_string()
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Send a JSON request body and map transport errors to `SendFailed`.
pub(crate) async fn send_json(
    request: reqwest::RequestBuilder,
    payload: &impl Serialize,
) -> Result<reqwest::Response, ChannelError> {
    request
        .json(payload)
        .send()
        .await
        .map_err(|e| ChannelError::SendFailed(e.to_string()))
}

/// Fail with `SendFailed` (embedding `context`, status, and body) when the
/// response status is not 2xx; otherwise pass the response through.
pub(crate) async fn ensure_success(
    response: reqwest::Response,
    context: &str,
) -> Result<reqwest::Response, ChannelError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable>".into());
    Err(ChannelError::SendFailed(format!(
        "{context} failed: {status} {body}"
    )))
}

/// Decode a Lark-style JSON envelope and apply its `code == 0` check.
pub(crate) async fn lark_envelope(
    response: reqwest::Response,
    context: &str,
) -> Result<Value, ChannelError> {
    let body: Value = response
        .json()
        .await
        .map_err(|e| ChannelError::SendFailed(e.to_string()))?;
    if body.get("code").and_then(|v| v.as_i64()) != Some(0) {
        let msg = body
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_error");
        return Err(ChannelError::SendFailed(format!(
            "{context} rejected: {msg}"
        )));
    }
    Ok(body)
}

/// Decode a Slack-style JSON envelope and apply its `ok == true` check.
pub(crate) async fn slack_envelope(
    response: reqwest::Response,
    context: &str,
) -> Result<Value, ChannelError> {
    let body: Value = response
        .json()
        .await
        .map_err(|e| ChannelError::SendFailed(e.to_string()))?;
    if body.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        let error = body
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown_error");
        return Err(ChannelError::SendFailed(format!(
            "{context} rejected: {error}"
        )));
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// Reconnect backoff
// ---------------------------------------------------------------------------

/// Fixed reconnect delay used by every provider after a failed or ended
/// connection/poll attempt.
// TODO: replace the fixed delay with a configurable (exponential) backoff policy.
pub(crate) async fn reconnect_delay() {
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
}

// ---------------------------------------------------------------------------
// WebSocket reconnect skeleton
// ---------------------------------------------------------------------------

/// A boxed, `Send` future: the return type of the reconnect loop's callbacks.
/// Boxed futures (the classic pre-`AsyncFn` adapter) let the reconnect skeleton
/// stay a single non-generic body while each provider's `open`/`serve` close
/// over its own config cheaply. The `Box::pin` happens once per connection
/// attempt, not per frame.
pub(crate) type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Outer reconnect loop shared by the socket-based providers (slack, discord,
/// lark): repeatedly "open a fresh WS URL, then run one connection until it
/// ends" until `running` flips off. The three provider-specific differences are
/// supplied as callbacks, so a socket channel only declares *how it gets a URL*
/// and *how it serves one connection* — the reconnect cadence is this layer's
/// concern, not each provider's.
///
/// - `open`  requests a fresh WebSocket URL (the provider's HTTP handshake).
/// - `serve` drives a single connection to completion; returning ends this
///   connection and the outer loop reconnects. It receives `&mut` access to
///   `state`, which survives across reconnects (lark's event deduper; `()` for
///   providers that need none).
///
/// See [`BoxFut`] for why the callbacks return boxed futures.
pub(crate) async fn ws_reconnect_loop<'a, State>(
    channel: &'static str,
    running: &'a Arc<AtomicBool>,
    state: &'a mut State,
    open: impl Fn() -> BoxFut<'a, Result<String, String>>,
    serve: impl for<'b> Fn(&'b mut State, String) -> BoxFut<'b, Result<(), String>>,
) {
    while running.load(Ordering::SeqCst) {
        match open().await {
            Ok(url) => {
                if let Err(err) = serve(state, url).await {
                    tracing::warn!(channel, error = %err, "socket connection ended");
                }
            }
            Err(err) => {
                tracing::warn!(channel, error = %err, "failed to open socket connection");
            }
        }

        if !running.load(Ordering::SeqCst) {
            break;
        }
        reconnect_delay().await;
    }
}

// ---------------------------------------------------------------------------
// Provider lifecycle skeleton
// ---------------------------------------------------------------------------

/// How `stop` disposes of the background task.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StopPolicy {
    /// Wait for the task to finish (telegram's poll loop exits on its own).
    Await,
    /// Abort the task immediately.
    Abort,
}

/// Shared lifecycle state for the built-in providers: the HTTP client, the
/// parsed config, the running flag, and the background task handle.
#[derive(Debug)]
pub(crate) struct Lifecycle<C> {
    pub(crate) http: reqwest::Client,
    pub(crate) running: Arc<AtomicBool>,
    pub(crate) config: Mutex<Option<C>>,
    pub(crate) task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<C: Clone> Lifecycle<C> {
    pub(crate) fn new() -> Self {
        Self::with_http(reqwest::Client::new())
    }

    pub(crate) fn with_http(http: reqwest::Client) -> Self {
        Self {
            http,
            running: Arc::new(AtomicBool::new(false)),
            config: Mutex::new(None),
            task: Mutex::new(None),
        }
    }

    /// Store the parsed config, flip `running`, and spawn the background loop.
    pub(crate) async fn begin<F>(&self, cfg: C, loop_fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        *self.config.lock().await = Some(cfg);
        self.running.store(true, Ordering::SeqCst);
        *self.task.lock().await = Some(tokio::spawn(loop_fut));
    }

    /// Snapshot of the current config, or `NotStarted` when stopped.
    pub(crate) async fn config(&self) -> Result<C, ChannelError> {
        self.config
            .lock()
            .await
            .clone()
            .ok_or(ChannelError::NotStarted)
    }

    /// Flip `running` off, dispose of the background task per `policy`, and
    /// drop the stored config.
    pub(crate) async fn stop(&self, policy: StopPolicy) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.task.lock().await.take() {
            match policy {
                StopPolicy::Await => {
                    let _ = handle.await;
                }
                StopPolicy::Abort => handle.abort(),
            }
        }
        *self.config.lock().await = None;
    }
}

impl<C: Clone> Default for Lifecycle<C> {
    fn default() -> Self {
        Self::new()
    }
}
