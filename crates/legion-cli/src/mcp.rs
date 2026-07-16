//! MCP inspection helpers for the CLI.
//!
//! These commands operate locally against the config file (mirroring
//! `skills.rs`): they do not require a running gateway. `reload` re-reads the
//! config and attempts to connect to each configured server, reporting which
//! connect — a connectivity check rather than an in-place gateway hot reload.

use crate::CliError;
use legion_core::config::Config;
use legion_mcp::{McpManager, McpTransport};

fn transport_kind(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio { .. } => "stdio",
        McpTransport::Http { .. } => "http",
        McpTransport::Sse { .. } => "sse",
        McpTransport::Ws { .. } => "ws",
    }
}

/// List MCP servers declared in `mcp.servers`.
pub async fn list(config: &Config) -> Result<(), CliError> {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        println!("No MCP servers configured (mcp.servers is empty).");
        return Ok(());
    }

    println!("{} MCP server(s) configured:\n", servers.len());
    for server in servers {
        println!("  name:        {}", server.name);
        println!("  transport:   {}", transport_kind(&server.transport));
        println!("  enabled:     {}", server.enabled);
        println!("  autoApprove: {}", server.auto_approve.len());
        println!("  timeoutMs:   {}", server.connect_timeout_ms);
        println!();
    }
    Ok(())
}

/// Connect to configured servers and list the tools they expose.
pub async fn tools(config: &Config) -> Result<(), CliError> {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        println!("No MCP servers configured (mcp.servers is empty).");
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
}
