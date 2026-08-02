//! MCP inspection and config-management helpers for the CLI.
//!
//! These commands operate locally against the config file (mirroring
//! `skills.rs`): they do not require a running gateway. `reload`/`status`
//! re-read the config and attempt to connect to each configured server,
//! reporting which connect — a connectivity check rather than an in-place
//! gateway hot reload. `add`/`remove`/`get` patch `mcp.servers` in the config
//! file through the helpers in [`crate::mcp_config`].

use crate::CliError;
use crate::mcp_config;
use legion_core::config::{Config, McpServerConfig};
use legion_mcp::{McpManager, McpTransport};
use std::collections::HashMap;
use std::path::Path;

/// One-line summary of how a server is reached, e.g. `stdio: npx -y fs` or
/// `http: https://example/rpc`.
pub(crate) fn transport_summary(transport: &McpTransport) -> String {
    match transport {
        McpTransport::Stdio { command, args, .. } => {
            let mut cmd = command.clone();
            for arg in args {
                cmd.push(' ');
                cmd.push_str(arg);
            }
            format!("stdio: {cmd}")
        }
        McpTransport::Http { url, .. } => format!("http: {url}"),
        McpTransport::Sse { url, .. } => format!("sse: {url}"),
        McpTransport::Ws { url, .. } => format!("ws: {url}"),
    }
}

fn print_add_hint() {
    println!("Add one with `legion mcp add <name> -- <command> [args...]` (stdio)");
    println!("or `legion mcp add <name> --transport http <url>`.");
}

/// List MCP servers declared in `mcp.servers`.
///
/// Config-only and therefore instant: live connection probing (which can block
/// on per-server connect timeouts) lives in [`status`].
pub async fn list(config: &Config) -> Result<(), CliError> {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        println!("No MCP servers configured (mcp.servers is empty).");
        print_add_hint();
        return Ok(());
    }

    println!("{} MCP server(s) configured:\n", servers.len());
    for server in servers {
        let state = if server.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!(
            "  {} ({}) — {}",
            server.name,
            state,
            transport_summary(&server.transport)
        );
        println!(
            "    autoApprove: {}, connectTimeoutMs: {}, toolTimeoutMs: {}",
            server.auto_approve.len(),
            server.connect_timeout_ms,
            server.tool_timeout_ms
        );
        if let Some(version) = &server.protocol_version {
            println!("    protocolVersion: {version}");
        }
    }
    println!("\nRun `legion mcp status` to probe live connections.");
    Ok(())
}

/// Connect to each enabled server and report live status: negotiated protocol
/// version and tool count on success, the error otherwise. Never fails on
/// unreachable servers — they are marked `✗`.
pub async fn status(config: &Config) -> Result<(), CliError> {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        println!("No MCP servers configured (mcp.servers is empty).");
        print_add_hint();
        return Ok(());
    }

    let mut manager = McpManager::new();
    let report = manager.load(servers).await;
    let connected: HashMap<String, legion_mcp::McpServerStatus> = manager
        .server_status()
        .into_iter()
        .map(|s| (s.server.clone(), s))
        .collect();
    let failed: HashMap<&str, &str> = report
        .failed
        .iter()
        .map(|(name, err)| (name.as_str(), err.as_str()))
        .collect();

    for server in servers {
        if !server.enabled {
            println!("  - {} (disabled)", server.name);
            continue;
        }
        match connected.get(&server.name) {
            Some(status) => println!(
                "  ✓ {} — protocol {}, {} tool(s)",
                server.name, status.protocol_version, status.tool_count
            ),
            None => {
                let err = failed
                    .get(server.name.as_str())
                    .copied()
                    .unwrap_or("not connected");
                println!("  ✗ {} — {err}", server.name);
            }
        }
    }
    manager.shutdown_all().await;
    Ok(())
}

/// Connect to configured servers and list the tools they expose.
pub async fn tools(config: &Config) -> Result<(), CliError> {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        println!("No MCP servers configured (mcp.servers is empty).");
        print_add_hint();
        return Ok(());
    }

    let mut manager = McpManager::new();
    let report = manager.load(servers).await;
    let discovered = manager.tools();

    if discovered.is_empty() {
        println!("No MCP tools discovered.");
    } else {
        println!("{} MCP tool(s) discovered:\n", discovered.len());
        for tool in discovered {
            println!("  {} — {}", tool.qualified_name(), tool.description());
        }
    }

    for (name, err) in &report.failed {
        eprintln!("warn  {name}: {err}");
    }
    manager.shutdown_all().await;
    Ok(())
}

/// Re-read `mcp.servers` and attempt to connect to each server, reporting which
/// connect. Returns an error if any enabled server fails to connect.
pub async fn reload(config: &Config) -> Result<(), CliError> {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        println!("No MCP servers configured (mcp.servers is empty).");
        print_add_hint();
        return Ok(());
    }

    let mut manager = McpManager::new();
    let report = manager.load(servers).await;
    manager.shutdown_all().await;

    for name in &report.connected {
        println!("ok   {name}");
    }
    for (name, err) in &report.failed {
        println!("err  {name}: {err}");
    }

    if report.connected.is_empty() && report.failed.is_empty() {
        println!("No enabled MCP servers.");
    } else if report.failed.is_empty() {
        println!("\n{} server(s) connected.", report.connected.len());
    } else {
        println!(
            "\n{} connected, {} failed.",
            report.connected.len(),
            report.failed.len()
        );
        return Err(CliError::Other(
            "one or more MCP servers failed to connect".to_string(),
        ));
    }
    Ok(())
}

/// CLI-independent inputs for `legion mcp add`, so the TUI `/mcp` command can
/// reuse the same validation path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddServerOptions {
    pub name: String,
    /// `stdio`, `http`, `sse`, or `ws`.
    pub transport: String,
    /// Server URL (http/sse/ws only).
    pub url: Option<String>,
    /// Stdio command followed by its arguments.
    pub command: Vec<String>,
    /// `KEY=VALUE` entries (stdio only).
    pub env: Vec<String>,
    /// `Name: value` entries (http/sse/ws only).
    pub headers: Vec<String>,
    pub auto_approve: Vec<String>,
    pub protocol_version: Option<String>,
    pub connect_timeout_ms: Option<u64>,
    pub tool_timeout_ms: Option<u64>,
}

fn parse_env(entries: &[String]) -> Result<HashMap<String, String>, CliError> {
    let mut env = HashMap::new();
    for entry in entries {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(CliError::Other(format!(
                "invalid --env '{entry}': expected KEY=VALUE"
            )));
        };
        if key.is_empty() {
            return Err(CliError::Other(format!(
                "invalid --env '{entry}': the key must not be empty"
            )));
        }
        env.insert(key.to_string(), value.to_string());
    }
    Ok(env)
}

fn parse_headers(entries: &[String]) -> Result<HashMap<String, String>, CliError> {
    let mut headers = HashMap::new();
    for entry in entries {
        let Some((name, value)) = entry.split_once(':') else {
            return Err(CliError::Other(format!(
                "invalid --header '{entry}': expected 'Name: value'"
            )));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(CliError::Other(format!(
                "invalid --header '{entry}': the header name must not be empty"
            )));
        }
        headers.insert(name.to_string(), value.trim().to_string());
    }
    Ok(headers)
}

/// Validate `opts` and build the corresponding [`McpServerConfig`].
pub fn build_server_config(opts: &AddServerOptions) -> Result<McpServerConfig, CliError> {
    if opts.name.trim().is_empty() {
        return Err(CliError::Other(
            "MCP server name must not be empty".to_string(),
        ));
    }
    let transport = match opts.transport.as_str() {
        "stdio" => {
            if opts.url.is_some() {
                return Err(CliError::Other(
                    "the stdio transport takes its command after `--`; remove the URL argument"
                        .to_string(),
                ));
            }
            if !opts.headers.is_empty() {
                return Err(CliError::Other(
                    "--header is only valid with the http/sse/ws transports".to_string(),
                ));
            }
            let (command, args) = opts.command.split_first().ok_or_else(|| {
                CliError::Other(
                    "the stdio transport requires a command after `--`, e.g. \
                     `legion mcp add fs -- npx -y @mcp/server-fs`"
                        .to_string(),
                )
            })?;
            McpTransport::Stdio {
                command: command.clone(),
                args: args.to_vec(),
                env: parse_env(&opts.env)?,
            }
        }
        kind @ ("http" | "sse" | "ws") => {
            if !opts.command.is_empty() {
                return Err(CliError::Other(format!(
                    "the {kind} transport does not take a command; pass only the URL"
                )));
            }
            if !opts.env.is_empty() {
                return Err(CliError::Other(
                    "--env is only valid with the stdio transport".to_string(),
                ));
            }
            let url = opts.url.clone().ok_or_else(|| {
                CliError::Other(format!(
                    "the {kind} transport requires a URL, e.g. \
                     `legion mcp add {} --transport {kind} https://example/rpc`",
                    opts.name
                ))
            })?;
            let headers = parse_headers(&opts.headers)?;
            match kind {
                "http" => McpTransport::Http { url, headers },
                "sse" => McpTransport::Sse { url, headers },
                _ => McpTransport::Ws { url, headers },
            }
        }
        other => {
            return Err(CliError::Other(format!(
                "unknown transport '{other}': expected stdio, http, sse, or ws"
            )));
        }
    };

    Ok(McpServerConfig {
        name: opts.name.clone(),
        transport,
        enabled: true,
        auto_approve: opts.auto_approve.clone(),
        connect_timeout_ms: opts.connect_timeout_ms.unwrap_or(15_000),
        protocol_version: opts.protocol_version.clone(),
        tool_timeout_ms: opts.tool_timeout_ms.unwrap_or(60_000),
    })
}

/// Add a server to the config file (`mcp.servers`).
pub fn add(config_path: &Path, opts: &AddServerOptions) -> Result<(), CliError> {
    let server = build_server_config(opts)?;
    mcp_config::add_server(config_path, &server)?;
    println!(
        "Added MCP server '{}' ({}) to {}",
        server.name,
        transport_summary(&server.transport),
        config_path.display()
    );
    Ok(())
}

/// Remove a server from the config file, printing what was removed.
pub fn remove(config_path: &Path, name: &str) -> Result<(), CliError> {
    let server = mcp_config::get_server(config_path, name)?
        .ok_or_else(|| CliError::Other(format!("no MCP server named '{name}' is configured")))?;
    mcp_config::remove_server(config_path, name)?;
    println!(
        "Removed MCP server '{}' ({}) from {}",
        name,
        transport_summary(&server.transport),
        config_path.display()
    );
    Ok(())
}

/// Print one server's full config as pretty JSON.
pub fn get(config_path: &Path, name: &str) -> Result<(), CliError> {
    let server = mcp_config::get_server(config_path, name)?
        .ok_or_else(|| CliError::Other(format!("no MCP server named '{name}' is configured")))?;
    println!("{}", serde_json::to_string_pretty(&server)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_core::config::Config;

    fn config_with_servers(servers_json: &str) -> Config {
        let json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "mcp": {{ "servers": {servers_json} }}
            }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[tokio::test]
    async fn list_handles_empty_servers() {
        let config = config_with_servers("[]");
        list(&config).await.unwrap();
    }

    #[tokio::test]
    async fn list_handles_configured_server() {
        let config = config_with_servers(
            r#"[{ "name": "fs", "type": "http", "url": "http://example/rpc" }]"#,
        );
        list(&config).await.unwrap();
    }

    #[tokio::test]
    async fn reload_reports_connect_failure() {
        let config = config_with_servers(
            r#"[{ "name": "down", "type": "http", "url": "http://127.0.0.1:1/rpc", "connectTimeoutMs": 200 }]"#,
        );
        let result = reload(&config).await;
        assert!(result.is_err(), "expected reload to report connect failure");
    }

    #[tokio::test]
    async fn status_marks_unreachable_server_without_failing() {
        let config = config_with_servers(
            r#"[
                { "name": "down", "type": "http", "url": "http://127.0.0.1:1/rpc", "connectTimeoutMs": 200 },
                { "name": "off", "type": "http", "url": "http://127.0.0.1:1/rpc", "enabled": false }
            ]"#,
        );
        status(&config).await.unwrap();
    }

    fn stdio_opts() -> AddServerOptions {
        AddServerOptions {
            name: "fs".to_string(),
            transport: "stdio".to_string(),
            command: vec![
                "npx".to_string(),
                "-y".to_string(),
                "@mcp/server-fs".to_string(),
            ],
            env: vec!["TOKEN=abc".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn build_stdio_server_from_command() {
        let server = build_server_config(&stdio_opts()).unwrap();
        assert_eq!(
            server.transport,
            McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@mcp/server-fs".to_string()],
                env: HashMap::from([("TOKEN".to_string(), "abc".to_string())]),
            }
        );
        assert!(server.enabled);
        assert_eq!(server.connect_timeout_ms, 15_000);
        assert_eq!(server.tool_timeout_ms, 60_000);
    }

    #[test]
    fn build_stdio_requires_command() {
        let opts = AddServerOptions {
            command: vec![],
            ..stdio_opts()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("requires a command"));
    }

    #[test]
    fn build_stdio_rejects_url() {
        let opts = AddServerOptions {
            url: Some("http://example/rpc".to_string()),
            ..stdio_opts()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("remove the URL argument"));
    }

    #[test]
    fn build_http_server_from_url_and_headers() {
        let opts = AddServerOptions {
            name: "web".to_string(),
            transport: "http".to_string(),
            url: Some("https://example/rpc".to_string()),
            headers: vec!["Authorization: Bearer t".to_string()],
            auto_approve: vec!["search".to_string()],
            protocol_version: Some("2025-06-18".to_string()),
            tool_timeout_ms: Some(5_000),
            ..Default::default()
        };
        let server = build_server_config(&opts).unwrap();
        assert_eq!(
            server.transport,
            McpTransport::Http {
                url: "https://example/rpc".to_string(),
                headers: HashMap::from([("Authorization".to_string(), "Bearer t".to_string())]),
            }
        );
        assert_eq!(server.auto_approve, vec!["search".to_string()]);
        assert_eq!(server.protocol_version.as_deref(), Some("2025-06-18"));
        assert_eq!(server.tool_timeout_ms, 5_000);
    }

    #[test]
    fn build_http_requires_url() {
        let opts = AddServerOptions {
            name: "web".to_string(),
            transport: "sse".to_string(),
            ..Default::default()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("requires a URL"));
    }

    #[test]
    fn build_http_rejects_command_and_env() {
        let opts = AddServerOptions {
            name: "web".to_string(),
            transport: "http".to_string(),
            url: Some("https://example/rpc".to_string()),
            command: vec!["npx".to_string()],
            ..Default::default()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("does not take a command"));

        let opts = AddServerOptions {
            name: "web".to_string(),
            transport: "http".to_string(),
            url: Some("https://example/rpc".to_string()),
            env: vec!["A=B".to_string()],
            ..Default::default()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("--env is only valid"));
    }

    #[test]
    fn build_rejects_bad_env_and_header() {
        let opts = AddServerOptions {
            env: vec!["NOEQUALS".to_string()],
            ..stdio_opts()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("expected KEY=VALUE"));

        let opts = AddServerOptions {
            name: "web".to_string(),
            transport: "http".to_string(),
            url: Some("https://example/rpc".to_string()),
            headers: vec!["NoColon".to_string()],
            ..Default::default()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("expected 'Name: value'"));
    }

    #[test]
    fn build_rejects_unknown_transport_and_empty_name() {
        let opts = AddServerOptions {
            name: "x".to_string(),
            transport: "carrier-pigeon".to_string(),
            ..Default::default()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("unknown transport"));

        let opts = AddServerOptions {
            name: "  ".to_string(),
            ..stdio_opts()
        };
        let err = build_server_config(&opts).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn add_remove_get_round_trip_on_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legion.json");
        std::fs::write(
            &path,
            r#"{ "gateway": { "auth": { "token": "x" } }, "mcp": { "servers": [] } }"#,
        )
        .unwrap();

        add(&path, &stdio_opts()).unwrap();
        get(&path, "fs").unwrap();
        remove(&path, "fs").unwrap();

        let err = remove(&path, "fs").unwrap_err();
        assert!(err.to_string().contains("no MCP server named 'fs'"));
    }
}
