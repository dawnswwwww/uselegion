//! PTY harness for Legion TUI integration tests.
//!
//! Spawns the `legion` binary inside a pseudo-terminal, drives it with
//! injected keystrokes, and captures the screen state via a small VTE parser.

#![allow(dead_code)]

pub mod screen;

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

pub use screen::{Parser, Screen};

/// Raw key byte constants for terminal input injection.
pub mod keys {
    pub const ENTER: &[u8] = b"\r";
    pub const CTRL_C: &[u8] = b"\x03";
    pub const CTRL_R: &[u8] = b"\x12";
    pub const ESC: &[u8] = b"\x1b";
    pub const UP: &[u8] = b"\x1b[A";
    pub const DOWN: &[u8] = b"\x1b[B";
    pub const LEFT: &[u8] = b"\x1b[D";
    pub const RIGHT: &[u8] = b"\x1b[C";
}

/// Controller for a single PTY session.
pub struct TuiPty {
    child: Box<dyn portable_pty::Child + Send>,
    writer: Box<dyn Write + Send>,
    reader_rx: mpsc::Receiver<Vec<u8>>,
    parser: Parser,
    #[allow(dead_code)]
    master: Box<dyn portable_pty::MasterPty + Send>,
}

impl TuiPty {
    /// Spawn `binary` inside a fresh PTY with a sanitized environment.
    pub fn spawn(binary: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let mut cmd = CommandBuilder::new(binary);
        for arg in args {
            cmd.arg(*arg);
        }
        apply_child_env(&mut cmd, env);

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn child in PTY")?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;
        let reader_rx = spawn_reader(reader);

        Ok(Self {
            child,
            writer,
            reader_rx,
            parser: Parser::new(),
            master: pair.master,
        })
    }

    /// Send raw bytes to the PTY stdin.
    pub fn send_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .context("failed to write to PTY")
    }

    /// Send printable text without a trailing newline.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_bytes(text.as_bytes())
    }

    /// Send a single ASCII letter, optionally with Shift held.
    pub fn send_key(&mut self, ch: char) -> Result<()> {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        self.send_bytes(s.as_bytes())
    }

    /// Send a newline/return.
    pub fn send_enter(&mut self) -> Result<()> {
        self.send_bytes(keys::ENTER)
    }

    /// Send Ctrl+C.
    pub fn send_ctrl_c(&mut self) -> Result<()> {
        self.send_bytes(keys::CTRL_C)
    }

    /// Drain any output that has already arrived and update the screen parser.
    pub fn drain(&mut self) {
        while let Ok(chunk) = self.reader_rx.try_recv() {
            self.parser.feed(&chunk);
        }
    }

    /// Wait until `text` appears on the parsed screen, or the timeout expires.
    pub fn wait_for_text(&mut self, text: &str, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.drain();
            if self.parser.screen().contains(text) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "timeout waiting for {:?} on screen. current screen:\n{}",
                    text,
                    self.parser.screen().screen_string()
                );
            }
            match self.reader_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => self.parser.feed(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.drain();
                    if self.parser.screen().contains(text) {
                        return Ok(());
                    }
                    anyhow::bail!(
                        "PTY reader disconnected before text {:?} appeared. screen:\n{}",
                        text,
                        self.parser.screen().screen_string()
                    );
                }
            }
        }
    }

    /// Return the current parsed screen contents.
    pub fn screen_string(&mut self) -> String {
        self.drain();
        self.parser.screen().screen_string()
    }

    /// Return a reference to the parsed screen.
    pub fn screen(&mut self) -> &Screen {
        self.drain();
        self.parser.screen()
    }

    /// Check whether the child process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait up to `timeout` for the child to exit, returning its exit code.
    pub fn wait_exit_code(&mut self, timeout: Duration) -> Option<u32> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status.exit_code()),
                Ok(None) if std::time::Instant::now() >= deadline => return None,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
    }

    /// Send Ctrl+C and wait for the process to exit.
    pub fn quit(&mut self, timeout: Duration) -> Result<()> {
        let _ = self.send_ctrl_c();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) if std::time::Instant::now() >= deadline => {
                    self.child.kill().context("failed to kill child")?;
                    self.child.wait().context("failed to wait for child")?;
                    return Ok(());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => anyhow::bail!("failed to wait for child: {e}"),
            }
        }
    }
}

impl Drop for TuiPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Host-terminal identity markers stripped from the child environment so the
/// child does not inherit the test runner's terminal quirks.
const HOST_TERMINAL_ENV_VARS: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERMINAL_EMULATOR",
    "WEZTERM_VERSION",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERM_SESSION_ID",
    "KITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "VTE_VERSION",
    "WT_SESSION",
    "TMUX",
    "TMUX_PANE",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "STY",
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "VIM_TERMINAL",
    "INSIDE_EMACS",
];

fn apply_child_env(cmd: &mut CommandBuilder, env: &[(&str, &str)]) {
    cmd.env("TERM", "xterm-256color");
    for var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
        cmd.env_remove(var);
    }
    for var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
        cmd.env_remove(var);
    }
    for var in HOST_TERMINAL_ENV_VARS {
        cmd.env_remove(var);
    }
    for &(key, val) in env {
        cmd.env(key, val);
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .expect("failed to spawn pty-reader thread");
    rx
}
