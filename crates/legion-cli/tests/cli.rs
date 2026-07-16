use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;

#[test]
fn should_print_help() {
    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Legion agent harness CLI"));
}

#[test]
fn should_print_gateway_subcommand_help() {
    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args(["gateway", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Start the Gateway"));
}

#[test]
fn should_print_agent_subcommand_help() {
    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args(["agent", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Send a single agent turn"));
}

#[test]
fn should_print_cron_subcommand_help() {
    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args(["cron", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Manage scheduled cron jobs"));
}

#[test]
fn should_print_tasks_subcommand_help() {
    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args(["tasks", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("View background task records"));
}

#[test]
fn should_validate_valid_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("legion.json");
    let mut file = std::fs::File::create(&config_path).unwrap();
    write!(
        file,
        r#"{{ "gateway": {{ "auth": {{ "token": "secret" }} }} }}"#
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args([
        "config",
        "validate",
        "--config",
        config_path.to_str().unwrap(),
    ]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("config is valid"));
}

#[test]
fn should_reject_invalid_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("legion.json");
    std::fs::write(&config_path, "not json").unwrap();

    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args([
        "config",
        "validate",
        "--config",
        config_path.to_str().unwrap(),
    ]);
    cmd.assert().failure();
}

#[test]
fn should_print_setup_subcommand_help() {
    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args(["setup", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("first-time setup wizard"));
}

#[test]
fn should_reject_unsafe_auth_mode_none() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("legion.json");
    std::fs::write(
        &config_path,
        r#"{ "gateway": { "bindHost": "0.0.0.0", "auth": { "mode": "none" } } }"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.args([
        "config",
        "validate",
        "--config",
        config_path.to_str().unwrap(),
    ]);
    cmd.assert().failure();
}
