//! Legion CLI client library.
//!
//! Provides shared helpers for config management, WebSocket communication with
//! the Gateway, and command handlers used by the `legion` binary.

pub mod costs;
pub mod driver;
pub mod gateway_manager;
pub mod goal;
pub mod loop_cmd;
pub mod mcp;
pub mod setup;
pub mod shell_commands;
pub mod skills;
pub mod slash_commands;
pub mod tui;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use legion_core::config::Config;
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, protocol::Message},
};

#[derive(Debug, Error)]
pub enum CliError {
    #[error("config error: {0}")]
    Config(#[from] legion_core::config::ConfigError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("gateway error: {0}")]
    Gateway(String),
    #[error("gateway manager error: {0}")]
    GatewayManager(#[from] crate::gateway_manager::GatewayManagerError),
    #[error("not connected")]
    NotConnected,
    /// The user cancelled an interactive flow (Esc / Ctrl-C).
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

/// Default path to the Legion config file (`~/.legion/legion.json`).
pub fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".legion").join("legion.json"))
}

/// Path used to track a running background Gateway process (`~/.legion/legion.pid`).
pub fn pid_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".legion").join("legion.pid"))
}

/// Path to the Gateway log file (`~/.legion/gateway.log`).
pub fn gateway_log_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".legion").join("gateway.log"))
}

/// Resolve the configured Gateway WebSocket URL.
pub fn gateway_ws_url(config: &Config) -> String {
    format!(
        "ws://{}:{}/ws",
        config.gateway.bind_host, config.gateway.port
    )
}

/// Resolve the configured Gateway HTTP URL.
pub fn gateway_http_url(config: &Config) -> String {
    format!(
        "http://{}:{}",
        config.gateway.bind_host, config.gateway.port
    )
}

/// Load the config from the default path.
///
/// Fails with guidance when no config exists yet: run `legion setup` first.
/// Earlier versions silently wrote a hard-coded MiniMax template with a
/// predictable gateway token — that produced confusing provider errors and a
/// weak default secret.
pub fn load_config() -> Result<Config, CliError> {
    match default_config_path() {
        Some(path) if path.exists() => {
            let text = std::fs::read_to_string(&path)?;
            if path.extension().is_some_and(|ext| ext == "json5") {
                Ok(Config::from_json5(&text)?)
            } else {
                Ok(Config::from_json(&text)?)
            }
        }
        Some(path) => Err(CliError::Other(format!(
            "configuration not found at {}; run `legion setup` first",
            path.display()
        ))),
        None => Err(CliError::Other(
            "unable to determine the config path; run `legion setup` first".to_string(),
        )),
    }
}

/// Validate a config file at the given path.
pub fn validate_config(path: &PathBuf) -> Result<(), CliError> {
    let text = std::fs::read_to_string(path)?;
    if path.extension().is_some_and(|ext| ext == "json5") {
        Config::from_json5(&text)?;
    } else {
        Config::from_json(&text)?;
    }
    Ok(())
}

/// Read a dotted key from the config file.
pub fn config_get(path: &PathBuf, key: &str) -> Result<Option<serde_json::Value>, CliError> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    Ok(get_nested(&value, key))
}

/// Set a dotted key in the config file.
pub fn config_set(
    path: &PathBuf,
    key: &str,
    raw_value: &str,
) -> Result<serde_json::Value, CliError> {
    let text = std::fs::read_to_string(path)?;
    let mut value: serde_json::Value = serde_json::from_str(&text)?;
    let parsed: serde_json::Value = serde_json::from_str(raw_value)
        .or_else(|_| Ok::<_, serde_json::Error>(json!(raw_value)))?;
    set_nested(&mut value, key, parsed)?;
    let out = serde_json::to_string_pretty(&value)?;
    std::fs::write(path, out)?;
    Ok(value)
}

fn get_nested(value: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    let mut current = value;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    Some(current.clone())
}

fn set_nested(
    value: &mut serde_json::Value,
    key: &str,
    new_value: serde_json::Value,
) -> Result<(), CliError> {
    let mut current = value;
    let mut parts = key.split('.').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if let Some(obj) = current.as_object_mut() {
                obj.insert(part.to_string(), new_value.clone());
                return Ok(());
            }
            return Err(CliError::Other(format!("key '{}' is not an object", key)));
        }
        current = current
            .as_object_mut()
            .and_then(|obj| obj.get_mut(part))
            .ok_or_else(|| CliError::Other(format!("key '{}' not found", key)))?;
    }
    Ok(())
}

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWrite = SplitSink<WsStream, Message>;
type WsRead = SplitStream<WsStream>;

/// A connected Gateway WebSocket client.
///
/// The underlying stream is split so that sending and receiving can happen
/// concurrently from different tasks.
pub struct GatewayClient {
    write: Arc<tokio::sync::Mutex<WsWrite>>,
    read: Arc<tokio::sync::Mutex<WsRead>>,
    next_id: AtomicU64,
    /// Warning produced when the gateway's reported version or protocol
    /// revision is incompatible with this CLI.
    version_warning: Option<String>,
    /// Protocol compatibility reported by the gateway in the hello payload, if any.
    gateway_info: Option<legion_protocol::ProtocolCompatibility>,
}

impl GatewayClient {
    /// Connect to the Gateway and perform the `connect` handshake.
    pub async fn connect(config: &Config) -> Result<Self, CliError> {
        let url = gateway_ws_url(config);
        let mut request = url
            .into_client_request()
            .map_err(|e| CliError::WebSocket(e.to_string()))?;
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_static("http://localhost"));

        let (stream, _response) = connect_async(request)
            .await
            .map_err(|e| CliError::WebSocket(e.to_string()))?;

        let (write, read) = stream.split();
        let mut client = Self {
            write: Arc::new(tokio::sync::Mutex::new(write)),
            read: Arc::new(tokio::sync::Mutex::new(read)),
            next_id: AtomicU64::new(1),
            version_warning: None,
            gateway_info: None,
        };
        let id = client.next_id();

        let cli_protocol = gateway_manager::GatewayManager::cli_compatibility();
        let connect_frame = json!({
            "type": "connect",
            "id": id,
            "params": {
                "auth": { "token": config.gateway.auth.token.clone().unwrap_or_default() },
                "deviceId": "legion-cli",
                "platform": std::env::consts::OS,
                "deviceFamily": "client",
                "role": "client",
                "protocol": cli_protocol
            }
        });

        client.send_json(&connect_frame).await?;
        let response = client.recv_json().await?.ok_or(CliError::NotConnected)?;

        if response.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let error = response
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("connect handshake failed");
            return Err(CliError::Gateway(error.to_string()));
        }

        // Version handshake: a stale background gateway may predate the CLI.
        // Prefer the machine-readable protocol block; fall back to crate version.
        let payload = response.get("payload");
        let gateway_protocol = payload.and_then(|p| p.get("protocol")).and_then(|v| {
            serde_json::from_value::<legion_protocol::ProtocolCompatibility>(v.clone()).ok()
        });

        if let Some(ref gw_protocol) = gateway_protocol {
            client.gateway_info = Some(gw_protocol.clone());
            if let Some(err) = cli_protocol.compatibility_error(gw_protocol) {
                client.version_warning = Some(err);
            }
        } else {
            let gateway_version = payload
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str());
            if gateway_version != Some(env!("CARGO_PKG_VERSION")) {
                client.version_warning = Some(match gateway_version {
                    Some(v) => format!(
                        "gateway version {v} differs from cli version {}; restart or upgrade the gateway",
                        env!("CARGO_PKG_VERSION")
                    ),
                    None => format!(
                        "gateway does not report a version (cli is {}); restart or upgrade the gateway",
                        env!("CARGO_PKG_VERSION")
                    ),
                });
            }
        }

        Ok(client)
    }

    /// Version mismatch warning detected during the connect handshake, if any.
    pub fn version_warning(&self) -> Option<&str> {
        self.version_warning.as_deref()
    }

    /// Protocol compatibility reported by the gateway, if any.
    pub fn gateway_info(&self) -> Option<&legion_protocol::ProtocolCompatibility> {
        self.gateway_info.as_ref()
    }

    fn next_id(&self) -> String {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("cli-{}", id)
    }

    /// Send a JSON frame over the WebSocket.
    pub async fn send_json(&self, value: &serde_json::Value) -> Result<(), CliError> {
        let text = serde_json::to_string(value)?;
        let mut write = self.write.lock().await;
        write
            .send(Message::Text(text.into()))
            .await
            .map_err(|e| CliError::WebSocket(e.to_string()))
    }

    /// Receive the next JSON frame from the WebSocket.
    pub async fn recv_json(&self) -> Result<Option<serde_json::Value>, CliError> {
        let mut read = self.read.lock().await;
        loop {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(serde_json::from_str(&text)?));
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Err(err)) => return Err(CliError::WebSocket(err.to_string())),
                _ => continue,
            }
        }
    }

    /// Send a request frame and await the matching response.
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CliError> {
        let id = self.next_id();
        let frame = json!({
            "type": "req",
            "id": id,
            "method": method,
            "params": params
        });
        self.send_json(&frame).await?;

        loop {
            let msg = self.recv_json().await?.ok_or(CliError::NotConnected)?;
            if msg.get("type").and_then(|v| v.as_str()) == Some("res")
                && msg.get("id").and_then(|v| v.as_str()) == Some(&id)
            {
                return Ok(msg);
            }
        }
    }

    /// Send an `agent` request and stream events until the run ends.
    /// `yolo` auto-approves every tool prompt on the gateway side.
    pub async fn agent_turn(
        &self,
        message: &str,
        dump_prompts: bool,
        yolo: bool,
        session_key: &str,
        workspace: Option<&std::path::Path>,
    ) -> Result<(), CliError> {
        let id = self.next_id();
        let mut params = json!({
            "sessionKey": session_key,
            "message": { "role": "user", "content": message },
            "idempotencyKey": id,
            "wait": true,
            "dumpPrompts": dump_prompts,
            "yolo": yolo
        });
        if let Some(ws) = workspace {
            params["workspace"] = json!(ws);
        }
        let frame = json!({
            "type": "req",
            "id": id,
            "method": "agent",
            "params": params
        });
        self.send_json(&frame).await?;

        let mut run_id: Option<String> = None;
        let timeout = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let msg = self.recv_json().await?.ok_or(CliError::NotConnected)?;
                match msg.get("type").and_then(|v| v.as_str()) {
                    Some("res") if msg.get("id").and_then(|v| v.as_str()) == Some(&id) => {
                        if msg.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                            run_id = msg
                                .get("payload")
                                .and_then(|p| p.get("run_id"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                        } else {
                            let err = msg
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("agent request failed");
                            return Err(CliError::Gateway(err.to_string()));
                        }
                    }
                    Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some("agent") => {
                        if let Some(payload) = msg.get("payload") {
                            print_agent_event(payload)?;
                            if payload.get("stream").and_then(|v| v.as_str()) == Some("lifecycle")
                                && payload.get("phase").and_then(|v| v.as_str()) == Some("end")
                            {
                                return Ok(());
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        match timeout.await {
            Ok(result) => result,
            Err(_) => {
                eprintln!(
                    "\n(timeout waiting for agent run {})",
                    run_id.unwrap_or_default()
                );
                Ok(())
            }
        }
    }

    /// Close the WebSocket connection gracefully.
    pub async fn close(self) {
        let mut write = self.write.lock().await;
        let _ = write.close().await;
    }
}

/// Default session key used by `legion agent` when no `--session` is given.
/// Every invocation shares it, so one-shot turns keep continuous context.
pub const DEFAULT_CLI_SESSION_KEY: &str = "agent:main:dm:cli:default:direct:cli";

/// Resolve a `--session` value into a full session key.
///
/// A value starting with `agent:` is taken as a full session key verbatim
/// (after validation); anything else is treated as a peer id and embedded
/// into `agent:main:dm:<channel>:default:direct:<peer>`. Transcripts are
/// stored per agent + peer id, so a peer id is enough to resume a session
/// regardless of which channel originally created it.
///
/// Validation mirrors the gateway (`agent_rpc::parse_session_key` +
/// `session_tools::is_safe_peer_id`) so typos fail fast with a clear error
/// instead of silently starting a fresh, unpersisted session.
pub fn resolve_session_key_arg(session: &str, channel: &str) -> Result<String, CliError> {
    if let Some(rest) = session.strip_prefix("agent:") {
        let parts: Vec<&str> = session.split(':').collect();
        let valid_shape = parts.len() == 7
            && matches!(parts[5], "direct" | "group" | "thread")
            && is_safe_session_segment(parts[1])
            && is_safe_session_segment(parts[6]);
        if valid_shape {
            return Ok(session.to_string());
        }
        return Err(CliError::Other(format!(
            "invalid session key 'agent:{rest}': expected \
             agent:<agent>:<scope>:<channel>:<account>:<direct|group|thread>:<peer> \
             with agent/peer limited to ASCII alphanumerics plus '.', '_', '-'"
        )));
    }
    if !is_safe_session_segment(session) {
        return Err(CliError::Other(format!(
            "invalid session id '{session}': use the transcript file name from \
             ~/.legion/agents/<agent>/sessions/ (ASCII alphanumerics plus '.', '_', '-'), \
             or a full 'agent:...' session key"
        )));
    }
    Ok(format!("agent:main:dm:{channel}:default:direct:{session}"))
}

/// Whitelist check matching the gateway's `is_safe_peer_id` rules.
fn is_safe_session_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Extract the peer id (last segment) from a full session key.
pub fn session_peer_id(session_key: &str) -> &str {
    session_key.rsplit(':').next().unwrap_or(session_key)
}

/// Extract the agent id (second segment) from a full session key.
pub fn session_agent_id(session_key: &str) -> Option<&str> {
    let parts: Vec<&str> = session_key.split(':').collect();
    if parts.len() == 7 && parts[0] == "agent" {
        Some(parts[1])
    } else {
        None
    }
}

pub fn print_agent_event(payload: &serde_json::Value) -> Result<(), CliError> {
    let stream = payload
        .get("stream")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match stream {
        "lifecycle" => {
            let phase = payload.get("phase").and_then(|v| v.as_str()).unwrap_or("?");
            println!("[run {}]", phase);
        }
        "assistant" => {
            let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            print!("{}", delta);
            std::io::Write::flush(&mut std::io::stdout())?;
        }
        "tool" => {
            let state = payload.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            let default_tool = json!({});
            let tool = payload.get("tool_call").unwrap_or(&default_tool);
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            println!("\n[tool {}] {}", state, name);
        }
        "compaction" => {
            let summary = payload
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            println!("\n[compaction] {}", summary);
        }
        _ => {}
    }
    Ok(())
}

/// Start the Gateway.
///
/// By default the Gateway is spawned as a background process. Use
/// `foreground = true` to keep it in the current terminal.
pub async fn start_gateway(config_path: Option<PathBuf>, foreground: bool) -> Result<(), CliError> {
    start_gateway_with_options(config_path, foreground, false).await
}

/// Start the Gateway with optional on-demand installation.
pub async fn start_gateway_with_options(
    config_path: Option<PathBuf>,
    foreground: bool,
    allow_install: bool,
) -> Result<(), CliError> {
    let config_path = match config_path {
        Some(path) => path,
        None => {
            let path = default_config_path()
                .ok_or_else(|| CliError::Other("unable to determine config path".to_string()))?;
            if !path.exists() {
                return Err(CliError::Other(format!(
                    "configuration not found at {}; run `legion setup` first",
                    path.display()
                )));
            }
            path
        }
    };

    if let Some(auth_path) = ensure_auth_profiles_template()? {
        tracing::info!(path = %auth_path.display(), "ensured auth profiles template");
    }

    let config = load_config()?;
    let manager = gateway_manager::GatewayManager::default_manager()?;

    // If a gateway is already running, check compatibility and reuse it.
    if !foreground {
        match manager.running_gateway_info(&config).await {
            Ok(Some(running)) => {
                let cli_compat = gateway_manager::GatewayManager::cli_compatibility();
                if let Some(err) = cli_compat.compatibility_error(&running.info.protocol) {
                    return Err(CliError::Other(format!(
                        "gateway is running but incompatible: {err}; run `legion gateway upgrade --restart`"
                    )));
                }
                println!(
                    "gateway is already running (pid {:?}) version {} protocol {}",
                    running.pid,
                    running.info.protocol.product_version,
                    running.info.protocol.protocol_revision
                );
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "failed to probe running gateway");
            }
        }
    }

    // Resolve a local binary.
    let gateway_bin = match manager.find_gateway_binary() {
        Ok(path) => path,
        Err(gateway_manager::GatewayManagerError::NotInstalled) | Err(_) => {
            if allow_install {
                if let Some(url) = default_manifest_url(&config) {
                    println!("no compatible gateway installed; downloading from {}", url);
                    match manager
                        .install_from_manifest(&url, None, "stable", true)
                        .await
                    {
                        Ok(path) => path,
                        Err(e) => {
                            return Err(CliError::Other(format!(
                                "failed to install gateway: {e}; install manually with `legion gateway install --from <archive>`"
                            )));
                        }
                    }
                } else {
                    return Err(CliError::Other(
                        "no compatible gateway installed; run `legion gateway install --from <archive>` or set gateway.manifestUrl"
                            .to_string(),
                    ));
                }
            } else {
                return Err(CliError::Other(
                    "no compatible gateway installed; run `legion gateway install` first, or use `legion gateway start --install`"
                        .to_string(),
                ));
            }
        }
    };

    // Validate compatibility of the selected binary before starting.
    match manager.probe_version(&gateway_bin) {
        Ok(info) => {
            if let Some(err) = gateway_manager::GatewayManager::cli_compatibility()
                .compatibility_error(&info.protocol)
            {
                return Err(CliError::Other(format!(
                    "selected gateway is incompatible: {err}; install a compatible version"
                )));
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to probe selected gateway binary");
        }
    }

    if foreground {
        let status = Command::new(&gateway_bin)
            .arg("--config")
            .arg(&config_path)
            .status()
            .map_err(|e| CliError::Other(format!("failed to run gateway binary: {e}")))?;
        if status.success() {
            return Ok(());
        } else {
            return Err(CliError::Other(format!(
                "gateway exited with status {status}"
            )));
        }
    }

    // Background: spawn the gateway binary and detach.
    let mut cmd = Command::new(&gateway_bin);
    cmd.arg("--config").arg(&config_path);
    let log_path = gateway_log_path().unwrap_or_else(|| PathBuf::from("/tmp/legion-gateway.log"));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    cmd.stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Create a new session so the child is not killed when the parent terminal session ends.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    let pid = child.id();

    if let Some(pid_path) = pid_file_path() {
        if let Some(parent) = pid_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&pid_path, pid.to_string())?;
    }

    // Update current pointer with runtime metadata.
    if let Ok(mut pointer) = manager.current_pointer().ok().flatten().ok_or(()) {
        pointer.pid = Some(pid);
        pointer.started_at = Some(chrono::Utc::now());
        pointer.endpoint = Some(gateway_ws_url(&config));
        pointer.config_path_hash = Some(gateway_manager::GatewayManager::config_path_hash(
            &config_path,
        ));
        let _ = manager.set_current_pointer(&pointer);
    }

    println!("gateway started in background (pid {})", pid);
    Ok(())
}

/// Resolve the default release manifest URL.
///
/// Priority: `LEGION_RELEASES_URL` environment variable, `gateway.manifestUrl`
/// config value, then a placeholder built-in URL.
pub fn default_manifest_url(_config: &Config) -> Option<String> {
    if let Ok(url) = std::env::var("LEGION_RELEASES_URL") {
        return Some(url);
    }
    // Best-effort config lookup; schema does not yet define gateway.manifestUrl,
    // so fall back to reading the raw JSON value.
    let path = default_config_path()?;
    if let Ok(Some(value)) = crate::config_get(&path, "gateway.manifestUrl") {
        if let Some(url) = value.as_str() {
            return Some(url.to_string());
        }
    }
    None
}

/// Stop the running background Gateway process using the pid file.
pub fn stop_gateway() -> Result<(), CliError> {
    let pid_path = pid_file_path().ok_or_else(|| CliError::Other("no home dir".to_string()))?;
    if !pid_path.exists() {
        return Err(CliError::Other(
            "gateway is not running (no pid file)".to_string(),
        ));
    }

    let pid: u32 = std::fs::read_to_string(&pid_path)?
        .trim()
        .parse()
        .map_err(|_| CliError::Other(format!("invalid pid file: {}", pid_path.display())))?;

    #[cfg(unix)]
    {
        let mut cmd = std::process::Command::new("kill");
        cmd.arg(pid.to_string());
        let _ = cmd.output()?;
    }
    #[cfg(not(unix))]
    {
        return Err(CliError::Other(
            "gateway stop is only implemented on Unix in MVP".to_string(),
        ));
    }

    let _ = std::fs::remove_file(pid_path);
    Ok(())
}

/// Print the last `n` lines of the Gateway log file.
pub fn gateway_logs(n: usize) -> Result<(), CliError> {
    let path = gateway_log_path().ok_or_else(|| CliError::Other("no home dir".to_string()))?;
    if !path.exists() {
        println!("no gateway log file at {}", path.display());
        return Ok(());
    }

    let text = std::fs::read_to_string(&path)?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        println!("{}", line);
    }
    Ok(())
}

/// Check whether the background Gateway appears to be running.
pub fn gateway_status() -> Result<String, CliError> {
    let pid_path = pid_file_path().ok_or_else(|| CliError::Other("no home dir".to_string()))?;
    if !pid_path.exists() {
        return Ok("gateway is not running".to_string());
    }

    let pid: u32 = std::fs::read_to_string(&pid_path)?
        .trim()
        .parse()
        .map_err(|_| CliError::Other(format!("invalid pid file: {}", pid_path.display())))?;

    if process_alive(pid) {
        Ok(format!("gateway is running (pid {})", pid))
    } else {
        let _ = std::fs::remove_file(&pid_path);
        Ok("gateway is not running (stale pid file removed)".to_string())
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// Return the PID of a running gateway if the pid file points to a live process.
/// Removes stale pid files and returns `None` in that case.
pub fn existing_gateway_pid() -> Option<u32> {
    let pid_path = pid_file_path()?;
    if !pid_path.exists() {
        return None;
    }

    let pid_text = std::fs::read_to_string(&pid_path).ok()?;
    let pid = pid_text.trim().parse::<u32>().ok()?;

    if process_alive(pid) {
        Some(pid)
    } else {
        let _ = std::fs::remove_file(&pid_path);
        None
    }
}

/// Print the latest prompt-dump record for a session
/// (`~/.legion/dump-prompts/<session>.jsonl`, written when `promptDump.enabled`
/// or `legion agent --dump-prompts` is on). Local file read; the Gateway does
/// not need to be running.
pub fn show_context(session: &str) -> Result<(), CliError> {
    let dir = dirs::home_dir()
        .map(|h| h.join(".legion").join("dump-prompts"))
        .ok_or_else(|| CliError::Other("unable to determine home directory".to_string()))?;
    let record = latest_dump_record(&dir, session)?;
    print!("{}", render_dump_record(&record, session));
    Ok(())
}

/// Read the last non-empty JSONL record from `<dir>/<session>.jsonl`.
fn latest_dump_record(dir: &std::path::Path, session: &str) -> Result<serde_json::Value, CliError> {
    let path = dir.join(format!("{}.jsonl", session.replace(['/', '\\'], "_")));
    let content = std::fs::read_to_string(&path).map_err(|e| {
        CliError::Other(format!(
            "no prompt dump for session '{session}' at {}: {e}",
            path.display()
        ))
    })?;
    let last = content
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| CliError::Other(format!("prompt dump {} is empty", path.display())))?;
    Ok(serde_json::from_str(last)?)
}

/// Render a prompt-dump record as a per-section token table.
fn render_dump_record(record: &serde_json::Value, session: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "session: {}\n",
        record["session"].as_str().unwrap_or(session)
    ));
    out.push_str(&format!("total tokens: {}\n", record["total_tokens"]));
    out.push_str(&format!(
        "cache prefix: {} bytes\n\n",
        record["cache_prefix_len"]
    ));
    out.push_str(&format!(
        "{:<20} {:<16} {:>8} {:>10}\n",
        "SECTION", "SOURCE", "TOKENS", "TRUNCATED"
    ));
    if let Some(sections) = record["sections"].as_array() {
        for s in sections {
            let id = s["id"].as_str().unwrap_or("?");
            let source = match &s["source"] {
                serde_json::Value::String(v) => v.clone(),
                serde_json::Value::Object(map) => map
                    .iter()
                    .next()
                    .map(|(k, v)| format!("{}:{}", k, v.as_str().unwrap_or("?")))
                    .unwrap_or_else(|| "?".to_string()),
                _ => "?".to_string(),
            };
            let tokens = s["tokens"].as_u64().unwrap_or(0);
            let truncated = s["truncated"].as_bool().unwrap_or(false);
            out.push_str(&format!(
                "{id:<20} {source:<16} {tokens:>8} {truncated:>10}\n"
            ));
        }
    }
    out
}

/// Default auth profiles template content.
pub fn default_auth_profiles_json() -> &'static str {
    r#"{
  "profiles": {
    "minimax-default": {
      "type": "api_key",
      "key": "YOUR_MINIMAX_API_KEY_HERE"
    }
  }
}
"#
}

/// Ensure the default auth profiles directory and template file exist.
pub fn ensure_auth_profiles_template() -> Result<Option<PathBuf>, CliError> {
    let path = dirs::home_dir().map(|h| h.join(".legion/agents/main/agent/auth-profiles.json"));

    if let Some(ref path) = path {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, default_auth_profiles_json())?;
        }
    }
    Ok(path)
}

/// Run health checks against the Gateway.
pub async fn doctor() -> Result<(), CliError> {
    let config = load_config()?;
    let url = gateway_http_url(&config);
    println!("checking gateway at {}", url);

    match reqwest::get(format!("{}/__legion__/canvas/", url)).await {
        Ok(resp) => println!("gateway http reachable: {}", resp.status()),
        Err(err) => println!("gateway http unreachable: {}", err),
    }

    match GatewayClient::connect(&config).await {
        Ok(client) => {
            let resp = client.request("health", json!({})).await?;
            println!("gateway ws health: {:?}", resp.get("payload"));
            client.close().await;
        }
        Err(err) => println!("gateway ws unreachable: {}", err),
    }

    // Sandbox availability (local platform checks).
    use legion_tools::sandbox::{SandboxMode, sandbox_available};
    for mode in [SandboxMode::Restricted, SandboxMode::Cube] {
        match sandbox_available(mode) {
            Ok(()) => println!("sandbox {:?}: available", mode),
            Err(reason) => println!("sandbox {:?}: unavailable ({})", mode, reason),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_dump_record_formats_section_table() {
        let record = json!({
            "ts": 1,
            "session": "agent:main:dm:cli:default:direct:cli",
            "sections": [
                { "id": "base", "source": "custom", "tokens": 42, "truncated": false },
                { "id": "outputStyle", "source": { "agent": "a1" }, "tokens": 7, "truncated": true }
            ],
            "total_tokens": 49,
            "cache_prefix_len": 128
        });
        let out = render_dump_record(&record, "fallback");
        assert!(out.contains("session: agent:main:dm:cli:default:direct:cli"));
        assert!(out.contains("total tokens: 49"));
        assert!(out.contains("cache prefix: 128 bytes"));
        assert!(out.contains("base"));
        assert!(out.contains("custom"));
        assert!(out.contains("outputStyle"));
        assert!(out.contains("agent:a1"));
        assert!(out.contains("true"));
    }

    #[test]
    fn latest_dump_record_reads_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        std::fs::write(
            &path,
            "{\"session\":\"s1\",\"total_tokens\":1}\n{\"session\":\"s1\",\"total_tokens\":2}\n",
        )
        .unwrap();
        let record = latest_dump_record(dir.path(), "s1").unwrap();
        assert_eq!(record["total_tokens"], 2);
    }

    #[test]
    fn latest_dump_record_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = latest_dump_record(dir.path(), "nope").unwrap_err();
        assert!(
            err.to_string()
                .contains("no prompt dump for session 'nope'")
        );
    }

    #[test]
    fn resolve_session_key_arg_builds_key_from_peer_id() {
        assert_eq!(
            resolve_session_key_arg("tui-123-1", "tui").unwrap(),
            "agent:main:dm:tui:default:direct:tui-123-1"
        );
        assert_eq!(
            resolve_session_key_arg("cli", "cli").unwrap(),
            DEFAULT_CLI_SESSION_KEY
        );
    }

    #[test]
    fn resolve_session_key_arg_passes_full_key_through() {
        let key = "agent:work:dm:telegram:default:group:chat-42";
        assert_eq!(resolve_session_key_arg(key, "cli").unwrap(), key);
    }

    #[test]
    fn resolve_session_key_arg_rejects_unsafe_peer_id() {
        for bad in ["../evil", "a/b", "has space", ""] {
            assert!(
                resolve_session_key_arg(bad, "cli").is_err(),
                "peer id {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn resolve_session_key_arg_rejects_malformed_full_key() {
        // Wrong segment count, bad peer kind, unsafe segments.
        assert!(resolve_session_key_arg("agent:main:dm:cli", "cli").is_err());
        assert!(resolve_session_key_arg("agent:main:dm:cli:default:channel:cli", "cli").is_err());
        assert!(resolve_session_key_arg("agent:main:dm:cli:default:direct:a/b", "cli").is_err());
    }

    #[test]
    fn session_peer_id_extracts_last_segment() {
        assert_eq!(
            session_peer_id("agent:main:dm:tui:default:direct:tui-9"),
            "tui-9"
        );
    }
}
