//! Integration tests for the interactive `legion setup` wizard.
//!
//! assert_cmd pipes stdin, which is not a terminal, so the wizard takes its
//! text fallback path (numbered choices and plain `read_line` prompts). That
//! still exercises the full flow: provider selection, credential gathering,
//! existing-config handling, and file output.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Build a `legion` command running against an isolated HOME with all
/// provider API key environment variables removed, so prompt counts are
/// deterministic.
fn legion(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("legion").unwrap();
    cmd.env("HOME", home.path());
    for var in [
        "MINIMAX_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_REGION",
        "TELEGRAM_BOT_TOKEN",
        "SLACK_BOT_TOKEN",
        "SLACK_APP_TOKEN",
        "DISCORD_BOT_TOKEN",
        "LARK_APP_SECRET",
        "MATRIX_ACCESS_TOKEN",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn config_text(home: &TempDir) -> String {
    std::fs::read_to_string(home.path().join(".legion/legion.json")).unwrap()
}

fn auth_text(home: &TempDir) -> String {
    std::fs::read_to_string(
        home.path()
            .join(".legion/agents/main/agent/auth-profiles.json"),
    )
    .unwrap()
}

#[test]
fn interactive_setup_full_flow_via_piped_stdin() {
    let home = TempDir::new().unwrap();
    // 2 = OpenAI; key; default model; 2 = No (skip connection test);
    // default bind host; default port; 6 = Done (no channels); 2 = No (no daemon).
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("2\nsk-piped-key\n\n2\n\n\n6\n2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Provider: openai"));

    let config: serde_json::Value = serde_json::from_str(&config_text(&home)).unwrap();
    assert_eq!(config["agents"]["defaults"]["model"], "openai");
    assert!(config["models"]["providers"].get("openai").is_some());

    let auth: serde_json::Value = serde_json::from_str(&auth_text(&home)).unwrap();
    assert_eq!(auth["profiles"]["openai-default"]["key"], "sk-piped-key");

    assert!(home.path().join(".legion/workspace/AGENTS.md").is_file());
}

#[test]
fn interactive_setup_selects_provider_by_name() {
    let home = TempDir::new().unwrap();
    // "anthropic" (prefix match); key; default model; skip test; defaults;
    // no channels; no daemon.
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("anthropic\nsk-ant\n\n2\n\n\n6\n2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Provider: anthropic"));

    let config: serde_json::Value = serde_json::from_str(&config_text(&home)).unwrap();
    assert_eq!(
        config["models"]["aliases"]["anthropic"],
        "anthropic/claude-sonnet-4-5"
    );
}

#[test]
fn interactive_setup_rejects_empty_api_key() {
    let home = TempDir::new().unwrap();
    // 2 = OpenAI; empty key (re-prompted); real key; rest defaults.
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("2\n\nsk-real\n\n2\n\n\n6\n2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("An API key is required."));
}

#[test]
fn interactive_setup_keep_existing_config_changes_nothing() {
    let home = TempDir::new().unwrap();
    legion(&home)
        .args([
            "setup",
            "--non-interactive",
            "--provider",
            "minimax",
            "--api-key",
            "sk-one",
        ])
        .assert()
        .success();
    let before = config_text(&home);

    // Existing config prompt: empty answer keeps the default (Keep).
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Keeping the existing configuration; nothing changed.",
        ));

    assert_eq!(config_text(&home), before);
    assert!(!home.path().join(".legion/legion.json.bak").exists());
}

#[test]
fn interactive_setup_reconfigure_writes_backup_and_merges_auth() {
    let home = TempDir::new().unwrap();
    legion(&home)
        .args([
            "setup",
            "--non-interactive",
            "--provider",
            "minimax",
            "--api-key",
            "sk-one",
        ])
        .assert()
        .success();

    // 3 = Reconfigure; 2 = OpenAI; key; default model; skip test; defaults;
    // no channels; no daemon.
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("3\n2\nsk-two\n\n2\n\n\n6\n2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("backed up"));

    let backup = std::fs::read_to_string(home.path().join(".legion/legion.json.bak")).unwrap();
    assert!(backup.contains("minimax-openai"));

    let config: serde_json::Value = serde_json::from_str(&config_text(&home)).unwrap();
    assert!(config["models"]["providers"].get("openai").is_some());

    // The original provider's credentials survive the reconfigure.
    let auth: serde_json::Value = serde_json::from_str(&auth_text(&home)).unwrap();
    let profiles = auth["profiles"].as_object().unwrap();
    assert_eq!(profiles["minimax-default"]["key"], "sk-one");
    assert_eq!(profiles["openai-default"]["key"], "sk-two");
}

#[test]
fn interactive_setup_abort_leaves_files_untouched() {
    let home = TempDir::new().unwrap();
    legion(&home)
        .args([
            "setup",
            "--non-interactive",
            "--provider",
            "minimax",
            "--api-key",
            "sk-one",
        ])
        .assert()
        .success();
    let before = config_text(&home);

    // 4 = Abort on the existing-config prompt.
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("4\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("setup aborted"));

    assert_eq!(config_text(&home), before);
}

#[test]
fn interactive_setup_configures_telegram_channel() {
    let home = TempDir::new().unwrap();
    // 2 = OpenAI; key; default model; skip test; default host/port;
    // 1 = Telegram; bot token; bot username; DM allowlist; 6 = Done; 2 = No daemon.
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("2\nsk-chan\n\n2\n\n\n1\n123:ABC\nmybot\n12345\n6\n2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Channels: telegram"));

    let config: serde_json::Value = serde_json::from_str(&config_text(&home)).unwrap();
    let telegram = &config["channels"]["telegram"];
    assert_eq!(telegram["token"], "123:ABC");
    assert_eq!(telegram["botUsername"], "mybot");
    assert_eq!(telegram["access"]["dmPolicy"], "allowlist");
    assert_eq!(telegram["access"]["allowlist"][0], "12345");
}

#[test]
fn add_provider_flag_merges_without_rewriting() {
    let home = TempDir::new().unwrap();
    legion(&home)
        .args([
            "setup",
            "--non-interactive",
            "--provider",
            "minimax",
            "--api-key",
            "sk-one",
        ])
        .assert()
        .success();

    legion(&home)
        .args([
            "setup",
            "--non-interactive",
            "--add-provider",
            "--provider",
            "openai",
            "--api-key",
            "sk-two",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("merged into"));

    let config: serde_json::Value = serde_json::from_str(&config_text(&home)).unwrap();
    let providers = config["models"]["providers"].as_object().unwrap();
    assert!(providers.contains_key("minimax-openai"));
    assert!(providers.contains_key("openai"));
    // The default model stays on the original provider.
    assert_eq!(config["agents"]["defaults"]["model"], "minimax");
    // The merge writes a backup too.
    assert!(home.path().join(".legion/legion.json.bak").is_file());

    let auth: serde_json::Value = serde_json::from_str(&auth_text(&home)).unwrap();
    let profiles = auth["profiles"].as_object().unwrap();
    assert_eq!(profiles["minimax-default"]["key"], "sk-one");
    assert_eq!(profiles["openai-default"]["key"], "sk-two");
}

#[test]
fn interactive_add_provider_menu_choice_merges() {
    let home = TempDir::new().unwrap();
    legion(&home)
        .args([
            "setup",
            "--non-interactive",
            "--provider",
            "minimax",
            "--api-key",
            "sk-one",
        ])
        .assert()
        .success();

    // 2 = Add provider; 2 = OpenAI; key; default model; skip test.
    let mut cmd = legion(&home);
    cmd.arg("setup")
        .write_stdin("2\n2\nsk-two\n\n2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("merged into"));

    let config: serde_json::Value = serde_json::from_str(&config_text(&home)).unwrap();
    assert!(config["models"]["providers"].get("openai").is_some());
    assert!(
        config["models"]["providers"]
            .get("minimax-openai")
            .is_some()
    );
    assert_eq!(config["agents"]["defaults"]["model"], "minimax");
}

#[test]
fn agent_without_config_points_at_setup() {
    let home = TempDir::new().unwrap();
    let mut cmd = legion(&home);
    cmd.args(["agent", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run `legion setup` first"));
    // No config file may be created as a side effect.
    assert!(!home.path().join(".legion/legion.json").exists());
}

#[test]
fn gateway_start_without_config_points_at_setup() {
    let home = TempDir::new().unwrap();
    let mut cmd = legion(&home);
    cmd.args(["gateway", "start"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run `legion setup` first"));
    assert!(!home.path().join(".legion/legion.json").exists());
}
