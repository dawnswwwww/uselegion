use futures::{SinkExt, StreamExt};
use legion_core::config::Config;
use legion_gateway::Gateway;
use serde_json::{Value, json};
use serial_test::serial;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "test-token";
const WORKSPACE_NAME: &str = "e2e-workspace";

fn build_config() -> Config {
    let json = json!({
        "gateway": {
            "bindHost": "127.0.0.1",
            "port": 0,
            "auth": { "mode": "token", "token": TOKEN }
        },
        "agents": {
            "defaults": {
                "workspace": WORKSPACE_NAME,
                "model": "minimax"
            }
        },
        "models": {
            "providers": {
                "minimax-openai": {
                    "id": "minimax-openai",
                    "kind": "openai",
                    "baseUrl": "https://api.minimaxi.com/v1",
                    "authProfile": "minimax-default",
                    "defaultModel": "MiniMax-M3"
                }
            },
            "aliases": {
                "minimax": "minimax-openai/MiniMax-M3"
            }
        },
        "tools": {
            "exec": { "approval": "off" }
        }
    });

    Config::from_json(&json.to_string()).expect("valid config")
}

fn setup_workspace(temp_dir: &TempDir) -> PathBuf {
    let workspace = temp_dir.path().join(WORKSPACE_NAME);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "You are a helpful assistant.").unwrap();
    std::fs::write(workspace.join("SOUL.md"), "Be concise.").unwrap();
    workspace
}

fn setup_auth_profiles(temp_dir: &TempDir, api_key: &str) {
    let auth_dir = temp_dir.path().join(".legion/agents/main/agent");
    std::fs::create_dir_all(&auth_dir).unwrap();
    let profiles = json!({
        "profiles": {
            "minimax-default": { "type": "api_key", "key": api_key }
        }
    });
    std::fs::write(auth_dir.join("auth-profiles.json"), profiles.to_string()).unwrap();
}

fn parse_frame(text: &str) -> Value {
    serde_json::from_str(text).expect("valid json")
}

#[tokio::test]
#[serial]
#[ignore = "requires MINIMAX_API_KEY env var"]
async fn e2e_minimax_tool_roundtrip() {
    let api_key = std::env::var("MINIMAX_API_KEY")
        .expect("MINIMAX_API_KEY env var must be set to run this test");

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let _workspace = setup_workspace(&temp_dir);
    setup_auth_profiles(&temp_dir, &api_key);

    let old_cwd = std::env::current_dir().expect("failed to get cwd");
    let old_home = std::env::var("HOME").ok();

    std::env::set_current_dir(temp_dir.path()).expect("failed to set cwd");
    unsafe { std::env::set_var("HOME", temp_dir.path()) };

    let config = build_config();
    let gateway = Gateway::new(config)
        .await
        .expect("failed to create gateway");
    let (addr, handle, shutdown_tx) = gateway
        .start_bound()
        .await
        .expect("failed to start gateway");

    let result = tokio::time::timeout(Duration::from_secs(120), run_test(addr)).await;

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    std::env::set_current_dir(old_cwd).expect("failed to restore cwd");
    match old_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }

    match result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("test failed: {err}"),
        Err(_) => panic!("test timed out after 120 seconds"),
    }
}

async fn run_test(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("ws://{}/ws", addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

    let connect = json!({
        "type": "connect",
        "id": "conn-1",
        "params": {
            "auth": { "token": TOKEN },
            "deviceId": "test-device",
            "platform": "test",
            "deviceFamily": "client",
            "role": "client"
        }
    });
    ws.send(Message::Text(connect.to_string().into())).await?;

    let hello = ws.next().await.unwrap()?.into_text()?;
    let frame = parse_frame(&hello);
    assert!(
        frame["ok"].as_bool().unwrap_or(false),
        "connect failed: {frame:?}"
    );

    let req = json!({
        "type": "req",
        "id": "req-a",
        "method": "agent",
        "params": {
            "sessionKey": "agent:main:dm:webchat:default:direct:user1",
            "message": { "role": "user", "content": "List the files in the workspace directory using the exec tool" },
            "idempotencyKey": "e2e-minimax-tool-1"
        }
    });
    ws.send(Message::Text(req.to_string().into())).await?;

    let mut start_seen = false;
    let mut end_seen = false;
    let mut deltas: Vec<String> = Vec::new();
    let mut error_seen = false;

    while let Some(msg) = ws.next().await {
        let text = msg?.into_text()?;
        let frame = parse_frame(&text);
        eprintln!("event: {text}");

        if frame["type"] == "event" && frame["event"] == "agent" {
            let payload = &frame["payload"];
            match payload["stream"].as_str() {
                Some("lifecycle") => match payload["phase"].as_str() {
                    Some("start") => start_seen = true,
                    Some("end") => {
                        end_seen = true;
                        break;
                    }
                    Some("error") => {
                        error_seen = true;
                    }
                    _ => {}
                },
                Some("assistant") => {
                    if let Some(delta) = payload["delta"].as_str() {
                        deltas.push(delta.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    ws.close(None).await?;

    assert!(start_seen, "expected lifecycle start event");
    assert!(!error_seen, "agent run failed");
    assert!(end_seen, "expected lifecycle end event");
    let final_text = deltas.concat();
    assert!(
        !final_text.trim().is_empty(),
        "expected non-empty final answer"
    );
    eprintln!("final text: {final_text}");
    Ok(())
}
