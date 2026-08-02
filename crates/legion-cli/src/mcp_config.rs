//! Read-modify-write helpers for the `mcp.servers` section of the config file.
//!
//! These operate on the raw JSON value so unrelated sections survive untouched,
//! back up the file before writing, and validate the result against the
//! `Config` schema (via `setup::write_config_with_backup`). Plain JSON only —
//! a `.json5` config must be edited by hand.

use crate::CliError;
use crate::setup::{read_json_config, write_config_with_backup};
use legion_core::config::McpServerConfig;
use serde_json::Value;
use std::path::Path;

/// Mutable access to the `mcp.servers` array, creating the path if missing.
fn servers_array_mut(config: &mut Value) -> Result<&mut Vec<Value>, CliError> {
    let root = config
        .as_object_mut()
        .expect("read_json_config returns an object");
    let mcp = root
        .entry("mcp")
        .or_insert_with(|| Value::Object(Default::default()));
    if !mcp.is_object() {
        return Err(CliError::Other(
            "config key 'mcp' is not an object".to_string(),
        ));
    }
    let mcp = mcp.as_object_mut().expect("mcp is an object");
    let servers = mcp
        .entry("servers")
        .or_insert_with(|| Value::Array(Vec::new()));
    servers
        .as_array_mut()
        .ok_or_else(|| CliError::Other("config key 'mcp.servers' is not an array".to_string()))
}

/// Append a server to `mcp.servers`. Errors when a server with the same name
/// already exists.
pub fn add_server(config_path: &Path, server: &McpServerConfig) -> Result<(), CliError> {
    if server.name.trim().is_empty() {
        return Err(CliError::Other(
            "MCP server name must not be empty".to_string(),
        ));
    }
    let mut config = read_json_config(config_path)?;
    let servers = servers_array_mut(&mut config)?;
    if servers
        .iter()
        .any(|s| s.get("name").and_then(Value::as_str) == Some(server.name.as_str()))
    {
        return Err(CliError::Other(format!(
            "an MCP server named '{}' is already configured; remove it first with `legion mcp remove {}`",
            server.name, server.name
        )));
    }
    servers.push(serde_json::to_value(server)?);
    write_config_with_backup(config_path, &config)
}

/// Remove a server by name. Returns `Ok(false)` when no such server exists.
/// The `mcp` key is dropped entirely when its servers array becomes empty.
pub fn remove_server(config_path: &Path, name: &str) -> Result<bool, CliError> {
    let mut config = read_json_config(config_path)?;
    let removed = {
        let servers = servers_array_mut(&mut config)?;
        let before = servers.len();
        servers.retain(|s| s.get("name").and_then(Value::as_str) != Some(name));
        servers.len() != before
    };
    if !removed {
        return Ok(false);
    }
    let empty = config
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    if empty {
        config
            .as_object_mut()
            .expect("read_json_config returns an object")
            .remove("mcp");
    }
    write_config_with_backup(config_path, &config)?;
    Ok(true)
}

/// Flip a server's `enabled` flag in place. Returns `Ok(false)` when no such
/// server exists.
pub fn set_enabled(config_path: &Path, name: &str, enabled: bool) -> Result<bool, CliError> {
    let mut config = read_json_config(config_path)?;
    let servers = servers_array_mut(&mut config)?;
    let Some(server) = servers.iter_mut().find_map(|s| {
        let obj = s.as_object_mut()?;
        (obj.get("name").and_then(Value::as_str) == Some(name)).then_some(obj)
    }) else {
        return Ok(false);
    };
    server.insert("enabled".to_string(), Value::Bool(enabled));
    write_config_with_backup(config_path, &config)?;
    Ok(true)
}

/// Read one server's config by name, `None` when not configured.
pub fn get_server(config_path: &Path, name: &str) -> Result<Option<McpServerConfig>, CliError> {
    let config = read_json_config(config_path)?;
    let server = config
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(Value::as_array)
        .and_then(|servers| {
            servers
                .iter()
                .find(|s| s.get("name").and_then(Value::as_str) == Some(name))
        });
    match server {
        Some(value) => Ok(Some(serde_json::from_value(value.clone())?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_core::config::McpTransport;
    use std::collections::HashMap;

    fn write_config(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("legion.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn base_config(servers: &str) -> String {
        format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "models": {{ "providers": {{}} }},
                "mcp": {{ "servers": {servers} }}
            }}"#
        )
    }

    fn stdio_server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@mcp/server-fs".to_string()],
                env: HashMap::from([("TOKEN".to_string(), "abc".to_string())]),
            },
            enabled: true,
            auto_approve: vec!["read_file".to_string()],
            connect_timeout_ms: 15_000,
            protocol_version: None,
            tool_timeout_ms: 60_000,
        }
    }

    #[test]
    fn add_server_appends_and_preserves_unrelated_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, &base_config("[]"));
        add_server(&path, &stdio_server("fs")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["gateway"]["auth"]["token"], "x");
        assert!(value["models"]["providers"].is_object());
        let servers = value["mcp"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], "fs");
        assert_eq!(servers[0]["type"], "stdio");
        assert_eq!(servers[0]["command"], "npx");
        // Backup was written before the edit.
        assert!(path.with_extension("json.bak").exists());
    }

    #[test]
    fn add_server_creates_mcp_section_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, r#"{ "gateway": { "auth": { "token": "x" } } }"#);
        add_server(&path, &stdio_server("fs")).unwrap();
        let server = get_server(&path, "fs").unwrap().unwrap();
        assert_eq!(server.name, "fs");
    }

    #[test]
    fn add_server_rejects_duplicate_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, &base_config("[]"));
        add_server(&path, &stdio_server("fs")).unwrap();
        let err = add_server(&path, &stdio_server("fs")).unwrap_err();
        assert!(err.to_string().contains("already configured"));
    }

    #[test]
    fn add_server_rejects_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, &base_config("[]"));
        let err = add_server(&path, &stdio_server("  ")).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn remove_server_removes_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            &base_config(
                r#"[
                    { "name": "fs", "type": "stdio", "command": "npx" },
                    { "name": "web", "type": "http", "url": "http://example/rpc" }
                ]"#,
            ),
        );
        assert!(remove_server(&path, "fs").unwrap());
        assert!(get_server(&path, "fs").unwrap().is_none());
        assert!(get_server(&path, "web").unwrap().is_some());
    }

    #[test]
    fn remove_server_missing_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, &base_config("[]"));
        assert!(!remove_server(&path, "nope").unwrap());
    }

    #[test]
    fn remove_server_drops_mcp_key_when_servers_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            &base_config(r#"[{ "name": "fs", "type": "stdio", "command": "npx" }]"#),
        );
        assert!(remove_server(&path, "fs").unwrap());
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("mcp").is_none());
        assert_eq!(value["gateway"]["auth"]["token"], "x");
    }

    #[test]
    fn set_enabled_toggles_preserving_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, &base_config("[]"));
        add_server(&path, &stdio_server("fs")).unwrap();

        assert!(set_enabled(&path, "fs", false).unwrap());
        let server = get_server(&path, "fs").unwrap().unwrap();
        assert!(!server.enabled);
        assert_eq!(server.auto_approve, vec!["read_file".to_string()]);
        assert_eq!(
            server.transport,
            McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@mcp/server-fs".to_string()],
                env: HashMap::from([("TOKEN".to_string(), "abc".to_string())]),
            }
        );

        assert!(set_enabled(&path, "fs", true).unwrap());
        assert!(get_server(&path, "fs").unwrap().unwrap().enabled);
    }

    #[test]
    fn set_enabled_missing_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, &base_config("[]"));
        assert!(!set_enabled(&path, "nope", false).unwrap());
    }

    #[test]
    fn get_server_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, &base_config("[]"));
        assert!(get_server(&path, "nope").unwrap().is_none());
    }
}
