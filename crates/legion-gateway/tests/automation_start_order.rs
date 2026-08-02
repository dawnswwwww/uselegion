//! Regression test: the automation subsystem (cron scheduler, task runner,
//! heartbeat, hooks) must not start before the listener is bound. A duplicate
//! gateway process that fails to bind must never run scheduled jobs or create
//! automation state — otherwise every launchd/CLI retry would double-fire cron
//! jobs and leave orphaned "running" task records behind.
//!
//! The gateway reads its automation stores from `$HOME/.legion/automation`, so
//! the test runs with `HOME` pointed at a temp dir. `#[serial]` because it
//! mutates the process env.

use legion_core::config::Config;
use legion_gateway::Gateway;
use serial_test::serial;
use tokio::net::TcpListener;

#[test]
#[serial]
fn failed_bind_does_not_start_automation() {
    let home = tempfile::tempdir().unwrap();
    temp_env::with_var("HOME", Some(home.path()), || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Occupy the port the gateway will try to bind.
            let blocker = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = blocker.local_addr().unwrap().port();

            let config = Config::from_json(&format!(
                r#"{{ "gateway": {{ "bindHost": "127.0.0.1", "port": {port}, "auth": {{ "mode": "token", "token": "t" }} }} }}"#
            ))
            .unwrap();

            let gateway = Gateway::new(config).await.unwrap();
            let err = gateway
                .start_bound()
                .await
                .expect_err("bind must fail while the port is occupied");
            assert!(
                err.to_string().contains("Address already in use"),
                "unexpected error: {err}"
            );

            // Automation never started: the task store file was not created.
            let tasks = home.path().join(".legion").join("automation").join("tasks.jsonl");
            assert!(
                !tasks.exists(),
                "automation task store must not be created when bind fails"
            );
        });
    });
}
