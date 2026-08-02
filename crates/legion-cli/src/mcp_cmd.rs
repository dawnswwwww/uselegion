//! `/mcp` slash command: inspect and manage MCP servers from the TUI.
//!
//! Config mutations (`list`, `enable`, `disable`, `remove`, `add`) are
//! synchronous read-modify-write operations on the config file via
//! [`crate::mcp_config`]. Live queries (`status`, `tools`, `resources`,
//! `prompts`) are async: the handler spawns a task that connects a
//! throwaway [`McpManager`] and reports back through the local-notice
//! channel (`AppState::local_tx`), mirroring the fire-and-forget pattern of
//! `/goal`. The full-featured add flow lives in the CLI (`legion mcp add`);
//! the slash grammar is intentionally minimal.

use crate::mcp::{AddServerOptions, build_server_config, transport_summary};
use crate::mcp_config;
use crate::slash_commands::CommandResult;
use crate::tui::{AppState, LocalNotice, MessageRole};
use legion_core::config::{Config, McpServerConfig};
use legion_mcp::{McpManager, McpServerStatus};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Maximum entries shown for `/mcp tools|resources|prompts`.
const MAX_LISTED: usize = 50;
/// Maximum characters of a tool/resource description shown per line.
const DESC_MAX_CHARS: usize = 80;
/// Outer bound for one async MCP query so a hung server cannot strand the
/// notice forever.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

const USAGE: &str = "usage: /mcp [list] · /mcp status [name] · /mcp tools|resources|prompts <name> · /mcp enable|disable|remove <name> · /mcp add <name> stdio <command...> · /mcp add <name> <http|sse|ws> <url>";

const NO_CONFIG: &str = "no config file available; /mcp needs a legion config to inspect or modify";

/// Parsed `/mcp` subcommand.
#[derive(Debug, Clone, PartialEq)]
pub enum McpCommand {
    /// `/mcp` or `/mcp list`: config-only server listing.
    List,
    /// `/mcp status [name]`: live connection probe.
    Status { name: Option<String> },
    /// `/mcp tools <name>`: tools exposed by one server.
    Tools { name: String },
    /// `/mcp resources <name>`: resources exposed by one server.
    Resources { name: String },
    /// `/mcp prompts <name>`: prompts exposed by one server.
    Prompts { name: String },
    /// `/mcp enable <name>` / `/mcp disable <name>`.
    SetEnabled { name: String, enabled: bool },
    /// `/mcp remove <name>`.
    Remove { name: String },
    /// `/mcp add ...` (minimal grammar; see module docs).
    Add(AddServerOptions),
}

/// Parse the argument string of a `/mcp` command. `Err` is usage text to
/// show the user.
pub fn parse_args(args: &str) -> Result<McpCommand, String> {
    let mut tokens = args.split_whitespace();
    let Some(sub) = tokens.next() else {
        return Ok(McpCommand::List);
    };
    let named = |tokens: &mut std::str::SplitWhitespace| -> Result<String, String> {
        tokens
            .next()
            .map(str::to_string)
            .ok_or_else(|| USAGE.to_string())
    };
    match sub {
        "list" => Ok(McpCommand::List),
        "status" => Ok(McpCommand::Status {
            name: tokens.next().map(str::to_string),
        }),
        "tools" => Ok(McpCommand::Tools {
            name: named(&mut tokens)?,
        }),
        "resources" => Ok(McpCommand::Resources {
            name: named(&mut tokens)?,
        }),
        "prompts" => Ok(McpCommand::Prompts {
            name: named(&mut tokens)?,
        }),
        "enable" => Ok(McpCommand::SetEnabled {
            name: named(&mut tokens)?,
            enabled: true,
        }),
        "disable" => Ok(McpCommand::SetEnabled {
            name: named(&mut tokens)?,
            enabled: false,
        }),
        "remove" => Ok(McpCommand::Remove {
            name: named(&mut tokens)?,
        }),
        "add" => parse_add(tokens),
        _ => Err(USAGE.to_string()),
    }
}

/// `/mcp add <name> stdio <command...>` or `/mcp add <name> <http|sse|ws>
/// <url>`. Anything fancier is rejected with a pointer at the CLI.
fn parse_add(mut tokens: std::str::SplitWhitespace) -> Result<McpCommand, String> {
    let add_hint = || {
        "usage: /mcp add <name> stdio <command...> · /mcp add <name> <http|sse|ws> <url>\nfor the full options (env, headers, timeouts, ...) use `legion mcp add --help`"
            .to_string()
    };
    let Some(name) = tokens.next() else {
        return Err(add_hint());
    };
    let Some(kind) = tokens.next() else {
        return Err(add_hint());
    };
    let opts = match kind {
        "stdio" => {
            let command: Vec<String> = tokens.map(str::to_string).collect();
            if command.is_empty() {
                return Err(add_hint());
            }
            AddServerOptions {
                name: name.to_string(),
                transport: "stdio".to_string(),
                command,
                ..Default::default()
            }
        }
        transport @ ("http" | "sse" | "ws") => {
            let Some(url) = tokens.next() else {
                return Err(add_hint());
            };
            if tokens.next().is_some() {
                return Err(add_hint());
            }
            AddServerOptions {
                name: name.to_string(),
                transport: transport.to_string(),
                url: Some(url.to_string()),
                ..Default::default()
            }
        }
        _ => return Err(add_hint()),
    };
    Ok(McpCommand::Add(opts))
}

/// `/mcp` slash-command handler (registered in `slash_commands::builtins`).
pub(crate) fn cmd_mcp(state: &mut AppState, args: &str) -> CommandResult {
    let command = match parse_args(args) {
        Ok(command) => command,
        Err(usage) => {
            state.push_message(MessageRole::System, usage);
            return CommandResult::Handled;
        }
    };

    match command {
        McpCommand::List => cmd_list(state),
        McpCommand::SetEnabled { name, enabled } => cmd_set_enabled(state, &name, enabled),
        McpCommand::Remove { name } => cmd_remove(state, &name),
        McpCommand::Add(opts) => cmd_add(state, &opts),
        McpCommand::Status { name } => {
            cmd_query(state, "querying MCP server status...", move |path| {
                timed(run_status(path, name))
            });
        }
        McpCommand::Tools { name } => {
            cmd_query(state, "querying MCP tools...", move |path| {
                timed(run_tools(path, name))
            });
        }
        McpCommand::Resources { name } => {
            cmd_query(state, "querying MCP resources...", move |path| {
                timed(run_resources(path, name))
            });
        }
        McpCommand::Prompts { name } => {
            cmd_query(state, "querying MCP prompts...", move |path| {
                timed(run_prompts(path, name))
            });
        }
    }
    CommandResult::Handled
}

// ---------------------------------------------------------------------------
// Synchronous subcommands (config file read-modify-write)
// ---------------------------------------------------------------------------

fn cmd_list(state: &mut AppState) {
    let Some(path) = state.config_path.clone() else {
        state.push_message(MessageRole::System, NO_CONFIG);
        return;
    };
    match load_servers(&path) {
        Ok(servers) => state.push_message(MessageRole::System, format_server_list(&servers)),
        Err(err) => state.push_message(MessageRole::System, err),
    }
}

fn cmd_set_enabled(state: &mut AppState, name: &str, enabled: bool) {
    let Some(path) = state.config_path.clone() else {
        state.push_message(MessageRole::System, NO_CONFIG);
        return;
    };
    let message = match mcp_config::set_enabled(&path, name, enabled) {
        Ok(true) => {
            let word = if enabled { "enabled" } else { "disabled" };
            format!("MCP server '{name}' {word}; takes effect on next gateway/host restart")
        }
        Ok(false) => format!("no MCP server named '{name}'"),
        Err(err) => format!("failed to update MCP config: {err}"),
    };
    state.push_message(MessageRole::System, message);
}

fn cmd_remove(state: &mut AppState, name: &str) {
    let Some(path) = state.config_path.clone() else {
        state.push_message(MessageRole::System, NO_CONFIG);
        return;
    };
    let message = match mcp_config::remove_server(&path, name) {
        Ok(true) => {
            format!("removed MCP server '{name}'; takes effect on next gateway/host restart")
        }
        Ok(false) => format!("no MCP server named '{name}'"),
        Err(err) => format!("failed to update MCP config: {err}"),
    };
    state.push_message(MessageRole::System, message);
}

fn cmd_add(state: &mut AppState, opts: &AddServerOptions) {
    let Some(path) = state.config_path.clone() else {
        state.push_message(MessageRole::System, NO_CONFIG);
        return;
    };
    let message = match build_server_config(opts)
        .and_then(|server| mcp_config::add_server(&path, &server).map(|()| server))
    {
        Ok(server) => format!(
            "added MCP server '{}' ({}); takes effect on next gateway/host restart",
            server.name,
            transport_summary(&server.transport)
        ),
        Err(err) => format!("failed to add MCP server: {err}"),
    };
    state.push_message(MessageRole::System, message);
}

// ---------------------------------------------------------------------------
// Asynchronous subcommands (live queries via the local-notice channel)
// ---------------------------------------------------------------------------

/// Shared pre-flight + spawn for the async subcommands. `query` builds the
/// future from the config path; the result text is delivered as a system
/// message through the local-notice channel.
fn cmd_query<F>(state: &mut AppState, pending: &str, query: impl FnOnce(PathBuf) -> F)
where
    F: Future<Output = String> + Send + 'static,
{
    let Some(path) = state.config_path.clone() else {
        state.push_message(MessageRole::System, NO_CONFIG);
        return;
    };
    let Some(tx) = state.local_tx.clone() else {
        state.push_message(
            MessageRole::System,
            "async MCP queries are unavailable in this context",
        );
        return;
    };
    state.push_message(MessageRole::System, pending.to_string());
    let future = query(path);
    tokio::spawn(async move {
        let text = future.await;
        let _ = tx.send(LocalNotice { text });
    });
}

/// Bound any query so a hung server cannot strand the notice forever.
async fn timed(future: impl Future<Output = String>) -> String {
    match tokio::time::timeout(QUERY_TIMEOUT, future).await {
        Ok(text) => text,
        Err(_) => format!(
            "MCP query timed out after {} seconds",
            QUERY_TIMEOUT.as_secs()
        ),
    }
}

/// Read `mcp.servers` from the config file at `path`.
fn load_servers(path: &Path) -> Result<Vec<McpServerConfig>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let config = Config::from_json(&text)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    Ok(config.mcp.servers)
}

/// Look up one configured server by name; errors when it is unknown or
/// disabled (live queries only make sense for enabled servers).
fn load_named_server(path: &Path, name: &str) -> Result<McpServerConfig, String> {
    let servers = load_servers(path)?;
    match servers.into_iter().find(|s| s.name == name) {
        Some(server) if server.enabled => Ok(server),
        Some(_) => Err(format!("MCP server '{name}' is disabled")),
        None => Err(format!("no MCP server named '{name}'")),
    }
}

/// Connect a throwaway manager to `server`. Err text is the `✗` line when
/// the connect fails.
async fn connect_one(server: McpServerConfig) -> Result<McpManager, String> {
    let name = server.name.clone();
    let mut manager = McpManager::new();
    let report = manager.load(std::slice::from_ref(&server)).await;
    if let Some((_, err)) = report.failed.into_iter().find(|(n, _)| n == &name) {
        manager.shutdown_all().await;
        return Err(format!("✗ {name} — {err}"));
    }
    Ok(manager)
}

async fn run_status(path: PathBuf, name: Option<String>) -> String {
    let servers = match load_servers(&path) {
        Ok(servers) => servers,
        Err(err) => return err,
    };
    let servers = match name {
        Some(name) => match servers.into_iter().find(|s| s.name == name) {
            Some(server) => vec![server],
            None => return format!("no MCP server named '{name}'"),
        },
        None => servers,
    };
    if servers.is_empty() {
        return "no MCP servers configured (mcp.servers is empty)".to_string();
    }
    let mut manager = McpManager::new();
    let report = manager.load(&servers).await;
    let text = format_status(&servers, &manager.server_status(), &report.failed);
    manager.shutdown_all().await;
    text
}

async fn run_tools(path: PathBuf, name: String) -> String {
    let server = match load_named_server(&path, &name) {
        Ok(server) => server,
        Err(err) => return err,
    };
    let manager = match connect_one(server).await {
        Ok(manager) => manager,
        Err(err) => return err,
    };
    let tools: Vec<(String, String)> = manager
        .tools()
        .iter()
        .filter(|tool| tool.server() == name)
        .map(|tool| {
            (
                tool.qualified_name().to_string(),
                tool.description().to_string(),
            )
        })
        .collect();
    manager.shutdown_all().await;
    format_tool_list(&name, &tools)
}

async fn run_resources(path: PathBuf, name: String) -> String {
    let server = match load_named_server(&path, &name) {
        Ok(server) => server,
        Err(err) => return err,
    };
    let manager = match connect_one(server).await {
        Ok(manager) => manager,
        Err(err) => return err,
    };
    let result = manager.list_resources(&name).await;
    manager.shutdown_all().await;
    match result {
        Ok(resources) => {
            let entries: Vec<(String, String)> = resources.iter().map(resource_entry).collect();
            format_entry_list("resources", &name, &entries)
        }
        Err(err) => format!("✗ {name} — {err}"),
    }
}

async fn run_prompts(path: PathBuf, name: String) -> String {
    let server = match load_named_server(&path, &name) {
        Ok(server) => server,
        Err(err) => return err,
    };
    let manager = match connect_one(server).await {
        Ok(manager) => manager,
        Err(err) => return err,
    };
    let result = manager.list_prompts(&name).await;
    manager.shutdown_all().await;
    match result {
        Ok(prompts) => {
            let entries: Vec<(String, String)> = prompts.iter().map(prompt_entry).collect();
            format_entry_list("prompts", &name, &entries)
        }
        Err(err) => format!("✗ {name} — {err}"),
    }
}

// ---------------------------------------------------------------------------
// Pure formatting helpers (unit-tested without real servers)
// ---------------------------------------------------------------------------

/// `/mcp list` rendering: every configured server with its enabled flag,
/// transport summary, and the noteworthy knobs.
pub fn format_server_list(servers: &[McpServerConfig]) -> String {
    if servers.is_empty() {
        return "no MCP servers configured (mcp.servers is empty)\nadd one with `/mcp add <name> stdio <command...>` or see `legion mcp add --help`"
            .to_string();
    }
    let mut lines = vec![format!("{} MCP server(s) configured:", servers.len())];
    for server in servers {
        let state = if server.enabled {
            "enabled"
        } else {
            "disabled"
        };
        lines.push(format!(
            "  {} ({}) — {}",
            server.name,
            state,
            transport_summary(&server.transport)
        ));
        let mut details = format!("    autoApprove: {}", server.auto_approve.len());
        if let Some(version) = &server.protocol_version {
            details.push_str(&format!(", protocolVersion: {version}"));
        }
        details.push_str(&format!(
            ", connectTimeoutMs: {}, toolTimeoutMs: {}",
            server.connect_timeout_ms, server.tool_timeout_ms
        ));
        lines.push(details);
    }
    lines.push(
        "hint: `/mcp status` for live state · `legion mcp add --help` for full add options"
            .to_string(),
    );
    lines.join("\n")
}

/// `/mcp status` rendering: one line per configured server.
pub fn format_status(
    servers: &[McpServerConfig],
    statuses: &[McpServerStatus],
    failed: &[(String, String)],
) -> String {
    let connected: HashMap<&str, &McpServerStatus> = statuses
        .iter()
        .map(|status| (status.server.as_str(), status))
        .collect();
    let failed: HashMap<&str, &str> = failed
        .iter()
        .map(|(name, err)| (name.as_str(), err.as_str()))
        .collect();

    let mut lines = vec!["MCP server status:".to_string()];
    for server in servers {
        if !server.enabled {
            lines.push(format!("  - {} (disabled)", server.name));
            continue;
        }
        match connected.get(server.name.as_str()) {
            Some(status) => lines.push(format!(
                "  ✓ {} — protocol {}, {} tool(s)",
                server.name, status.protocol_version, status.tool_count
            )),
            None => {
                let err = failed
                    .get(server.name.as_str())
                    .copied()
                    .unwrap_or("not connected");
                lines.push(format!("  ✗ {} — {err}", server.name));
            }
        }
    }
    lines.join("\n")
}

/// `/mcp tools <name>` rendering, capped at [`MAX_LISTED`] entries.
pub fn format_tool_list(server: &str, tools: &[(String, String)]) -> String {
    if tools.is_empty() {
        return format!("server '{server}' exposes no tools");
    }
    let mut lines = vec![format!("{} tool(s) from '{server}':", tools.len())];
    for (qualified, description) in tools.iter().take(MAX_LISTED) {
        lines.push(format!(
            "  {qualified} — {}",
            truncate(description, DESC_MAX_CHARS)
        ));
    }
    if tools.len() > MAX_LISTED {
        lines.push(format!("  ... and {} more", tools.len() - MAX_LISTED));
    }
    lines.join("\n")
}

/// `/mcp resources|prompts <name>` rendering: `(identifier, detail)` pairs,
/// capped at [`MAX_LISTED`] entries.
pub fn format_entry_list(kind: &str, server: &str, entries: &[(String, String)]) -> String {
    if entries.is_empty() {
        return format!("server '{server}' exposes no {kind}");
    }
    let mut lines = vec![format!("{} {kind} from '{server}':", entries.len())];
    for (identifier, detail) in entries.iter().take(MAX_LISTED) {
        if detail.is_empty() {
            lines.push(format!("  {identifier}"));
        } else {
            lines.push(format!(
                "  {identifier} — {}",
                truncate(detail, DESC_MAX_CHARS)
            ));
        }
    }
    if entries.len() > MAX_LISTED {
        lines.push(format!("  ... and {} more", entries.len() - MAX_LISTED));
    }
    lines.join("\n")
}

/// Extract `(uri, detail)` from a `resources/list` entry.
fn resource_entry(value: &Value) -> (String, String) {
    let uri = value
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or("(no uri)")
        .to_string();
    let name = value.get("name").and_then(Value::as_str).unwrap_or("");
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    (uri, join_detail(name, description))
}

/// Extract `(name, detail)` from a `prompts/list` entry; the title (or
/// description) forms the detail.
fn prompt_entry(value: &Value) -> (String, String) {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("(unnamed)")
        .to_string();
    let title = value.get("title").and_then(Value::as_str).unwrap_or("");
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    (name, join_detail(title, description))
}

fn join_detail(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, true) => String::new(),
        (false, true) => first.to_string(),
        (true, false) => second.to_string(),
        (false, false) => format!("{first} — {second}"),
    }
}

/// Char-safe truncation with an ellipsis marker.
fn truncate(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut out: String = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash_commands::{CommandResult, dispatch};
    use legion_core::config::McpTransport;

    fn stdio_server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@mcp/server-fs".to_string()],
                env: HashMap::new(),
            },
            enabled: true,
            auto_approve: vec!["read_file".to_string()],
            connect_timeout_ms: 15_000,
            protocol_version: None,
            tool_timeout_ms: 60_000,
        }
    }

    fn write_config(dir: &tempfile::TempDir, servers: &str) -> PathBuf {
        let path = dir.path().join("legion.json");
        std::fs::write(
            &path,
            format!(
                r#"{{ "gateway": {{ "auth": {{ "token": "x" }} }}, "mcp": {{ "servers": {servers} }} }}"#
            ),
        )
        .unwrap();
        path
    }

    fn state_with_config(path: PathBuf) -> AppState {
        AppState {
            config_path: Some(path),
            ..Default::default()
        }
    }

    fn last_message(state: &AppState) -> &str {
        &state.messages().last().unwrap().content
    }

    // -- parse_args ---------------------------------------------------------

    #[test]
    fn parse_bare_and_list() {
        assert_eq!(parse_args("").unwrap(), McpCommand::List);
        assert_eq!(parse_args("list").unwrap(), McpCommand::List);
    }

    #[test]
    fn parse_status_with_optional_name() {
        assert_eq!(
            parse_args("status").unwrap(),
            McpCommand::Status { name: None }
        );
        assert_eq!(
            parse_args("status fs").unwrap(),
            McpCommand::Status {
                name: Some("fs".to_string())
            }
        );
    }

    #[test]
    fn parse_named_queries() {
        assert_eq!(
            parse_args("tools fs").unwrap(),
            McpCommand::Tools {
                name: "fs".to_string()
            }
        );
        assert_eq!(
            parse_args("resources fs").unwrap(),
            McpCommand::Resources {
                name: "fs".to_string()
            }
        );
        assert_eq!(
            parse_args("prompts fs").unwrap(),
            McpCommand::Prompts {
                name: "fs".to_string()
            }
        );
    }

    #[test]
    fn parse_enable_disable_remove() {
        assert_eq!(
            parse_args("enable fs").unwrap(),
            McpCommand::SetEnabled {
                name: "fs".to_string(),
                enabled: true
            }
        );
        assert_eq!(
            parse_args("disable fs").unwrap(),
            McpCommand::SetEnabled {
                name: "fs".to_string(),
                enabled: false
            }
        );
        assert_eq!(
            parse_args("remove fs").unwrap(),
            McpCommand::Remove {
                name: "fs".to_string()
            }
        );
    }

    #[test]
    fn parse_add_stdio_and_http() {
        assert_eq!(
            parse_args("add fs stdio npx -y @mcp/server-fs").unwrap(),
            McpCommand::Add(AddServerOptions {
                name: "fs".to_string(),
                transport: "stdio".to_string(),
                command: vec![
                    "npx".to_string(),
                    "-y".to_string(),
                    "@mcp/server-fs".to_string()
                ],
                ..Default::default()
            })
        );
        assert_eq!(
            parse_args("add web http https://example/rpc").unwrap(),
            McpCommand::Add(AddServerOptions {
                name: "web".to_string(),
                transport: "http".to_string(),
                url: Some("https://example/rpc".to_string()),
                ..Default::default()
            })
        );
    }

    #[test]
    fn parse_usage_errors() {
        for args in [
            "bogus",
            "tools",
            "resources",
            "prompts",
            "enable",
            "disable",
            "remove",
            "add",
            "add fs",
            "add fs stdio",
            "add fs http",
            "add fs http http://a extra",
            "add fs carrier-pigeon x",
        ] {
            let err = parse_args(args).unwrap_err();
            assert!(err.contains("usage:"), "args '{args}': {err}");
        }
    }

    // -- format_server_list -------------------------------------------------

    #[test]
    fn format_server_list_empty_hints_add() {
        let text = format_server_list(&[]);
        assert!(text.contains("no MCP servers configured"));
        assert!(text.contains("/mcp add"));
    }

    #[test]
    fn format_server_list_renders_servers() {
        let mut disabled = stdio_server("old");
        disabled.enabled = false;
        disabled.protocol_version = Some("2025-06-18".to_string());
        let text = format_server_list(&[stdio_server("fs"), disabled]);
        assert!(text.contains("2 MCP server(s) configured:"));
        assert!(text.contains("fs (enabled) — stdio: npx -y @mcp/server-fs"));
        assert!(text.contains("old (disabled)"));
        assert!(text.contains("autoApprove: 1"));
        assert!(text.contains("protocolVersion: 2025-06-18"));
        assert!(text.contains("/mcp status"));
    }

    // -- format_status ------------------------------------------------------

    fn status(server: &str, tools: usize) -> McpServerStatus {
        McpServerStatus {
            server: server.to_string(),
            protocol_version: "2025-06-18".to_string(),
            capabilities: Value::Null,
            tool_count: tools,
        }
    }

    #[test]
    fn format_status_marks_each_server() {
        let mut down = stdio_server("down");
        down.enabled = true;
        let mut off = stdio_server("off");
        off.enabled = false;
        let servers = vec![stdio_server("fs"), down, off];
        let text = format_status(
            &servers,
            &[status("fs", 3)],
            &[("down".to_string(), "connection refused".to_string())],
        );
        assert!(text.contains("✓ fs — protocol 2025-06-18, 3 tool(s)"));
        assert!(text.contains("✗ down — connection refused"));
        assert!(text.contains("- off (disabled)"));
    }

    #[test]
    fn format_status_missing_without_error_is_not_connected() {
        let text = format_status(&[stdio_server("fs")], &[], &[]);
        assert!(text.contains("✗ fs — not connected"));
    }

    // -- format_tool_list / format_entry_list -------------------------------

    #[test]
    fn format_tool_list_empty_and_capped() {
        assert_eq!(format_tool_list("fs", &[]), "server 'fs' exposes no tools");

        let tools: Vec<(String, String)> = (0..60)
            .map(|i| (format!("mcp__fs__tool{i}"), format!("does thing {i}")))
            .collect();
        let text = format_tool_list("fs", &tools);
        assert!(text.contains("60 tool(s) from 'fs':"));
        assert!(text.contains("mcp__fs__tool0 — does thing 0"));
        assert!(text.contains("... and 10 more"));
        assert!(!text.contains("tool50"));
    }

    #[test]
    fn format_tool_list_truncates_long_descriptions() {
        let long = "word ".repeat(50);
        let tools = vec![("mcp__fs__t".to_string(), long)];
        let text = format_tool_list("fs", &tools);
        let line = text.lines().nth(1).unwrap();
        assert!(line.ends_with('…'));
        assert!(line.chars().count() < DESC_MAX_CHARS + 20);
    }

    #[test]
    fn format_entry_list_empty_and_detail_variants() {
        assert_eq!(
            format_entry_list("resources", "fs", &[]),
            "server 'fs' exposes no resources"
        );
        assert_eq!(
            format_entry_list("prompts", "fs", &[]),
            "server 'fs' exposes no prompts"
        );
        let entries = vec![
            ("file:///a".to_string(), "a file".to_string()),
            ("file:///b".to_string(), String::new()),
        ];
        let text = format_entry_list("resources", "fs", &entries);
        assert!(text.contains("2 resources from 'fs':"));
        assert!(text.contains("file:///a — a file"));
        assert!(text.lines().any(|l| l == "  file:///b"));
    }

    // -- resource/prompt entry extraction ------------------------------------

    #[test]
    fn resource_and_prompt_entries_extract_fields() {
        let resource = serde_json::json!({
            "uri": "file:///x", "name": "x", "description": "the x file"
        });
        assert_eq!(
            resource_entry(&resource),
            ("file:///x".to_string(), "x — the x file".to_string())
        );
        let bare = serde_json::json!({ "uri": "file:///y" });
        assert_eq!(
            resource_entry(&bare),
            ("file:///y".to_string(), String::new())
        );

        let prompt = serde_json::json!({ "name": "review", "description": "review code" });
        assert_eq!(
            prompt_entry(&prompt),
            ("review".to_string(), "review code".to_string())
        );
        let titled = serde_json::json!({ "name": "p", "title": "P", "description": "d" });
        assert_eq!(
            prompt_entry(&titled),
            ("p".to_string(), "P — d".to_string())
        );
    }

    // -- dispatch-level ------------------------------------------------------

    #[test]
    fn dispatch_mcp_without_config_path_is_graceful() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/mcp");
        assert!(matches!(result, CommandResult::Handled));
        assert!(last_message(&state).contains("no config file"));
    }

    #[test]
    fn dispatch_mcp_status_without_config_path_is_graceful() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/mcp status");
        assert!(matches!(result, CommandResult::Handled));
        assert!(last_message(&state).contains("no config file"));
    }

    #[test]
    fn dispatch_mcp_unknown_subcommand_shows_usage() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/mcp frobnicate");
        assert!(matches!(result, CommandResult::Handled));
        assert!(last_message(&state).contains("usage: /mcp"));
    }

    #[test]
    fn dispatch_mcp_list_reads_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"[
                { "name": "fs", "type": "stdio", "command": "npx" },
                { "name": "web", "type": "http", "url": "http://example/rpc", "enabled": false }
            ]"#,
        );
        let mut state = state_with_config(path);
        let result = dispatch(&mut state, "/mcp");
        assert!(matches!(result, CommandResult::Handled));
        let text = last_message(&state);
        assert!(text.contains("fs (enabled)"));
        assert!(text.contains("web (disabled)"));
    }

    #[test]
    fn dispatch_mcp_enable_toggles_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"[{ "name": "fs", "type": "stdio", "command": "npx" }]"#,
        );
        let mut state = state_with_config(path.clone());

        let result = dispatch(&mut state, "/mcp disable fs");
        assert!(matches!(result, CommandResult::Handled));
        assert!(last_message(&state).contains("disabled"));
        assert!(last_message(&state).contains("restart"));
        assert!(
            !mcp_config::get_server(&path, "fs")
                .unwrap()
                .unwrap()
                .enabled
        );

        dispatch(&mut state, "/mcp enable fs");
        assert!(
            mcp_config::get_server(&path, "fs")
                .unwrap()
                .unwrap()
                .enabled
        );

        dispatch(&mut state, "/mcp enable nope");
        assert!(last_message(&state).contains("no MCP server named 'nope'"));
    }

    #[test]
    fn dispatch_mcp_remove_drops_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"[{ "name": "fs", "type": "stdio", "command": "npx" }]"#,
        );
        let mut state = state_with_config(path.clone());

        let result = dispatch(&mut state, "/mcp remove fs");
        assert!(matches!(result, CommandResult::Handled));
        assert!(last_message(&state).contains("removed MCP server 'fs'"));
        assert!(mcp_config::get_server(&path, "fs").unwrap().is_none());

        dispatch(&mut state, "/mcp remove fs");
        assert!(last_message(&state).contains("no MCP server named 'fs'"));
    }

    #[test]
    fn dispatch_mcp_add_writes_config_and_rejects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "[]");
        let mut state = state_with_config(path.clone());

        let result = dispatch(&mut state, "/mcp add fs stdio npx -y @mcp/server-fs");
        assert!(matches!(result, CommandResult::Handled));
        assert!(last_message(&state).contains("added MCP server 'fs'"));
        let server = mcp_config::get_server(&path, "fs").unwrap().unwrap();
        assert_eq!(
            server.transport,
            McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@mcp/server-fs".to_string()],
                env: HashMap::new(),
            }
        );

        dispatch(&mut state, "/mcp add web http https://example/rpc");
        assert!(last_message(&state).contains("added MCP server 'web'"));

        dispatch(&mut state, "/mcp add fs stdio npx");
        assert!(last_message(&state).contains("already configured"));
    }

    #[test]
    fn dispatch_mcp_add_fancy_grammar_points_at_cli() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "[]");
        let mut state = state_with_config(path);
        dispatch(&mut state, "/mcp add fs stdio");
        let text = last_message(&state);
        assert!(text.contains("usage: /mcp add"));
        assert!(text.contains("legion mcp add --help"));
    }
}
