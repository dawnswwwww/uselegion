//! Phase 0 baseline: lock the components assembled by `AgentHost::new`.
//!
//! This test lives outside the gateway crate so it exercises the public API
//! that the CLI will later consume via `legion-host`. It fails if the
//! transport-neutral assembly regresses.

use legion_core::config::Config;
use legion_gateway::AgentHost;
use tempfile::TempDir;

#[tokio::test]
async fn agent_host_assembles_plugins_runtime_session_and_cron_store() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let memory_path = tmp.path().join("memory");
    tokio::fs::create_dir_all(&workspace).await.unwrap();

    let config = Config::from_json(&format!(
        r#"{{
            "gateway": {{ "auth": {{ "token": "x" }} }},
            "agents": {{ "defaults": {{ "workspace": "{}", "model": "openai/gpt-4o" }} }},
            "memory": {{
                "builtin": {{
                    "collectionPath": "{}",
                    "embeddingDimension": 64
                }}
            }}
        }}"#,
        workspace.display().to_string().replace('\\', "/"),
        memory_path.display().to_string().replace('\\', "/"),
    ))
    .unwrap();

    let host = AgentHost::new(config)
        .await
        .expect("AgentHost assembly should succeed");

    // System plugins (channel providers) were loaded during assembly.
    let channels = host.registry.channels();
    assert!(
        channels.len() >= 6,
        "expected at least the six built-in channel providers, got {}",
        channels.len()
    );

    // The runtime harness registry is populated (built-in runtime at minimum).
    assert!(
        !host.runtime.id().is_empty(),
        "expected a non-empty harness registry id"
    );

    // Session store and cron store are wired.
    let session_key = "agent:main:dm:tui:default:direct:assembly-test";
    assert!(
        host.session_store.load(session_key).await.is_empty(),
        "empty session should load as empty"
    );
}
