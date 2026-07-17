//! PTY-based integration tests for the Legion TUI.
//!
//! These tests exercise the real `legion` binary inside a pseudo-terminal. They
//! intentionally avoid needing an API key by interacting with the setup prompt
//! and using Ctrl+C to exit.

mod pty_harness;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use pty_harness::TuiPty;
use tempfile::TempDir;

fn legion_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_legion"))
}

fn spawn_with_isolated_home<'a>(args: &[&'a str]) -> Result<(TuiPty, TempDir)> {
    let home = TempDir::new()?;
    let pty = TuiPty::spawn(
        &legion_binary(),
        args,
        &[
            ("HOME", home.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", ""),
        ],
    )?;
    Ok((pty, home))
}

#[test]
fn binary_shows_setup_prompt_when_not_configured() -> Result<()> {
    let (mut pty, _home) = spawn_with_isolated_home(&[])?;
    pty.wait_for_text("Legion has not been configured", Duration::from_secs(5))?;
    Ok(())
}

#[test]
fn setup_prompt_can_respond_no_and_exit() -> Result<()> {
    let (mut pty, _home) = spawn_with_isolated_home(&[])?;
    pty.wait_for_text("Legion has not been configured", Duration::from_secs(5))?;
    pty.send_key('n')?;
    pty.send_enter()?;
    pty.wait_for_text("Setup is required", Duration::from_secs(5))?;
    let code = pty.wait_exit_code(Duration::from_secs(5));
    assert_eq!(code, Some(1), "expected exit code 1 after declining setup");
    Ok(())
}

#[test]
fn can_interrupt_setup_with_ctrl_c() -> Result<()> {
    let (mut pty, _home) = spawn_with_isolated_home(&[])?;
    pty.wait_for_text("Legion has not been configured", Duration::from_secs(5))?;
    pty.send_ctrl_c()?;
    let code = pty.wait_exit_code(Duration::from_secs(5));
    assert!(code.is_some(), "expected process to exit after Ctrl+C");
    Ok(())
}
