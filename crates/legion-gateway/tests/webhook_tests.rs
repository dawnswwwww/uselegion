//! Integration tests for the `POST /webhook/{id}` cron trigger endpoint.
//!
//! The gateway reads its automation stores from `$HOME/.legion/automation`, so
//! each test runs with `HOME` pointed at a temp dir that contains a pre-seeded
//! `cron.jsonl`. Tests are `#[serial]` because they mutate the process env.

use hmac::Mac;
use legion_core::config::Config;
use legion_gateway::Gateway;
use serde_json::json;
use serial_test::serial;
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpListener;

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

/// Pre-seed a cron store with one webhook-only job and one plain cron job.
fn seed_cron_store(home: &Path) {
    let dir = home.join(".legion").join("automation");
    std::fs::create_dir_all(&dir).unwrap();
    let jobs = [
        json!({
            "id": "wh-test",
            "agent_id": "main",
            "message": "ping",
            "schedule": "__webhook__",
            "enabled": true,
            "created_at": "2026-01-01T00:00:00Z",
            "webhook_secret": "s3cret"
        }),
        json!({
            "id": "wh-plain",
            "agent_id": "main",
            "message": "ping",
            "schedule": "0 9 * * *",
            "enabled": true,
            "created_at": "2026-01-01T00:00:00Z"
        }),
    ];
    let mut content = String::new();
    for job in jobs {
        content.push_str(&serde_json::to_string(&job).unwrap());
        content.push('\n');
    }
    std::fs::write(dir.join("cron.jsonl"), content).unwrap();
}

fn test_config() -> Config {
    Config::from_json(
        r#"{ "gateway": { "bindHost": "127.0.0.1", "auth": { "mode": "token", "token": "test-token" } } }"#,
    )
    .unwrap()
}

/// Spawn the gateway on an ephemeral loopback port; returns its base URL and
/// the server task handle (abort it at the end of the test).
async fn spawn_test_gateway() -> (String, tokio::task::JoinHandle<()>) {
    let gateway = Gateway::new(test_config()).await.unwrap();
    let router = gateway.router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), server)
}

#[test]
#[serial]
fn webhook_endpoint_authorizes_and_triggers_jobs() {
    let home = tempfile::tempdir().unwrap();
    temp_env::with_var("HOME", Some(home.path()), || {
        seed_cron_store(home.path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let gateway = Gateway::new(test_config()).await.unwrap();
            let router = gateway.router();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap();
            let base = format!("http://{addr}");

            // Unknown job id -> 404.
            let resp = client
                .post(format!("{base}/webhook/no-such-job"))
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 404);

            // Job without a webhook secret -> 404 (do not leak job existence).
            let body = b"{}";
            let resp = client
                .post(format!("{base}/webhook/wh-plain"))
                .header("X-Hub-Signature-256", sign("s3cret", body))
                .body(body.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 404);

            // Invalid signature -> 401.
            let resp = client
                .post(format!("{base}/webhook/wh-test"))
                .header("X-Hub-Signature-256", "sha256=deadbeef")
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401);

            // Missing signature header -> 401.
            let resp = client
                .post(format!("{base}/webhook/wh-test"))
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401);

            // Valid signature -> 200 with a task id (the task itself fails:
            // the test config has no real provider, but the trigger succeeds).
            let body = br#"{"ref":"main"}"#;
            let resp = client
                .post(format!("{base}/webhook/wh-test"))
                .header("X-Hub-Signature-256", sign("s3cret", body))
                .body(body.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let payload: serde_json::Value = resp.json().await.unwrap();
            let task_id = payload.get("task_id").and_then(|v| v.as_str());
            assert!(task_id.is_some_and(|id| id.starts_with("task-cron-")));

            server.abort();
        });
    });
}

/// A signature that is valid for body A must not authorize posting body B:
/// this is the substitution attack HMAC verification defends against.
#[test]
#[serial]
fn webhook_rejects_signature_computed_over_different_body() {
    let home = tempfile::tempdir().unwrap();
    temp_env::with_var("HOME", Some(home.path()), || {
        seed_cron_store(home.path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (base, server) = spawn_test_gateway().await;
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap();

            let signed_body = br#"{"ref":"main"}"#;
            let posted_body = br#"{"ref":"main","injected":true}"#;
            let resp = client
                .post(format!("{base}/webhook/wh-test"))
                .header("X-Hub-Signature-256", sign("s3cret", signed_body))
                .body(posted_body.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401);

            server.abort();
        });
    });
}

/// A well-formed HMAC over the posted body, but keyed with a different
/// secret, must be rejected.
#[test]
#[serial]
fn webhook_rejects_valid_signature_with_wrong_secret() {
    let home = tempfile::tempdir().unwrap();
    temp_env::with_var("HOME", Some(home.path()), || {
        seed_cron_store(home.path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (base, server) = spawn_test_gateway().await;
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap();

            let body = br#"{"ref":"main"}"#;
            let resp = client
                .post(format!("{base}/webhook/wh-test"))
                .header("X-Hub-Signature-256", sign("attacker-secret", body))
                .body(body.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401);

            server.abort();
        });
    });
}
