//! PTY-based integration tests for `legion setup`'s interactive TTY path.
//!
//! Piping stdin (see `setup.rs`) exercises the text fallback; these tests
//! attach the child to a pseudo-terminal so the arrow-key selector and the
//! masked input run in raw mode. Unix only — skipped on Windows where the
//! wizard takes the same fallback path as piped stdin.

#![cfg(unix)]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(20);

/// Environment variables the wizard inspects; cleared so prompt counts stay
/// deterministic regardless of the developer's shell.
const CLEAN_ENV: &[&str] = &[
    "MINIMAX_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
    "TELEGRAM_BOT_TOKEN",
    "SLACK_BOT_TOKEN",
    "SLACK_APP_TOKEN",
    "DISCORD_BOT_TOKEN",
    "LARK_APP_SECRET",
    "MATRIX_ACCESS_TOKEN",
];

/// A minimal expect-style driver over a pty master.
struct PtySession {
    master: std::fs::File,
    transcript: Vec<u8>,
}

impl PtySession {
    /// Open a pty pair; returns (session over the master, slave for the child).
    fn open() -> (Self, OwnedFd) {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt failed");
            assert_eq!(libc::grantpt(master), 0, "grantpt failed");
            assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
            let name = libc::ptsname(master);
            assert!(!name.is_null(), "ptsname failed");
            let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
            assert!(slave >= 0, "failed to open the pty slave");

            // Give the child a real window size so width-dependent menu
            // rendering takes the normal path.
            let size = libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(libc::ioctl(master, libc::TIOCSWINSZ, &size), 0);

            // Non-blocking master: the expect loop polls.
            let flags = libc::fcntl(master, libc::F_GETFL);
            assert!(flags >= 0, "fcntl(F_GETFL) failed");
            assert_eq!(
                libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );

            (
                Self {
                    master: std::fs::File::from_raw_fd(master),
                    transcript: Vec::new(),
                },
                OwnedFd::from_raw_fd(slave),
            )
        }
    }

    /// Accumulate output until `needle` appears; panics with the transcript
    /// on timeout.
    fn expect(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        let mut buf = [0u8; 4096];
        loop {
            let text = String::from_utf8_lossy(&self.transcript).into_owned();
            if text.contains(needle) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?};\ntranscript so far:\n{text}"
            );
            match self.master.read(&mut buf) {
                Ok(0) => std::thread::sleep(Duration::from_millis(10)),
                Ok(n) => self.transcript.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // EIO: the child exited and closed the slave; keep polling
                // until the timeout in case the needle already arrived.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("pty read failed: {e}"),
            }
        }
    }

    fn send(&mut self, bytes: &str) {
        self.master.write_all(bytes.as_bytes()).unwrap();
    }

    fn transcript(&self) -> String {
        String::from_utf8_lossy(&self.transcript).into_owned()
    }
}

/// Wait for the child while draining the pty master.
///
/// macOS pty buffers are only a few KB; the wizard's final summary can fill
/// the buffer while nobody reads the master, and then the child blocks in
/// `exit()` flushing stdout — `Child::wait()` would hang forever.
fn wait_exit(pty: &mut PtySession, child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    let mut buf = [0u8; 4096];
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit in time; transcript:\n{}",
            pty.transcript()
        );
        match pty.master.read(&mut buf) {
            Ok(n) if n > 0 => pty.transcript.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Spawn `legion setup` attached to the pty slave as its controlling
/// terminal, so crossterm raw mode actually engages.
fn spawn_setup(home: &TempDir, slave: &OwnedFd) -> std::process::Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_legion"));
    cmd.arg("setup")
        .env("HOME", home.path())
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    for var in CLEAN_ENV {
        cmd.env_remove(var);
    }
    let slave_fd = slave.as_raw_fd();
    unsafe {
        // A controlling terminal is required for `/dev/tty`-based raw mode.
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().unwrap()
}

#[test]
fn pty_setup_arrow_keys_and_masked_input() {
    let home = TempDir::new().unwrap();
    let (mut pty, slave) = PtySession::open();
    let mut child = spawn_setup(&home, &slave);
    drop(slave);

    // Provider menu: ↓ once moves MiniMax → OpenAI; Enter confirms.
    pty.expect("Select a model provider:");
    pty.send("\x1b[B");
    pty.send("\r");

    // Masked API key entry.
    pty.expect("OpenAI API key (input is masked):");
    pty.send("sk-pty-123\r");

    pty.expect("Default model");
    pty.send("\r");

    // Connection test: 'n' picks No immediately in raw mode.
    pty.expect("Test the connection now?");
    pty.send("n");

    pty.expect("Bind host");
    pty.send("\r");
    pty.expect("Port");
    pty.send("\r");

    // Channel onboarding: default selection is "Done".
    pty.expect("Add a chat channel?");
    pty.send("\r");

    // Daemon install: default is No.
    pty.expect("background service");
    pty.send("\r");

    let status = wait_exit(&mut pty, &mut child);
    assert!(
        status.success(),
        "setup failed; transcript:\n{}",
        pty.transcript()
    );

    // Raw-mode rendering must terminate lines with \r\n (a bare \n renders
    // the menu as a staircase).
    assert!(
        pty.transcript.windows(2).any(|w| w == b"\r\n"),
        "no CRLF found in pty output"
    );
    // The key must have been masked: it appears nowhere in the transcript,
    // and the echo shows stars instead.
    let transcript = pty.transcript();
    assert!(
        !transcript.contains("sk-pty-123"),
        "API key was echoed in plain text"
    );
    assert!(
        transcript.contains("***"),
        "no masked echo found in transcript:\n{transcript}"
    );

    let config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".legion/legion.json")).unwrap(),
    )
    .unwrap();
    assert!(config["models"]["providers"].get("openai").is_some());
    assert_eq!(config["agents"]["defaults"]["model"], "openai");

    let auth: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            home.path()
                .join(".legion/agents/main/agent/auth-profiles.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth["profiles"]["openai-default"]["key"], "sk-pty-123");
}

#[test]
fn pty_setup_vertical_menu_arrows_then_abort() {
    let home = TempDir::new().unwrap();
    // Seed a config non-interactively first.
    let mut seed = Command::new(env!("CARGO_BIN_EXE_legion"));
    seed.args([
        "setup",
        "--non-interactive",
        "--provider",
        "minimax",
        "--api-key",
        "sk-seed",
    ])
    .env("HOME", home.path())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    for var in CLEAN_ENV {
        seed.env_remove(var);
    }
    assert!(seed.status().unwrap().success());
    let before = std::fs::read_to_string(home.path().join(".legion/legion.json")).unwrap();

    let (mut pty, slave) = PtySession::open();
    let mut child = spawn_setup(&home, &slave);
    drop(slave);

    // Vertical menu Keep / Add provider / Configure channels / Reconfigure /
    // Abort: ↓ ×4 lands on Abort; Enter confirms.
    pty.expect("What would you like to do?");
    pty.send("\x1b[B\x1b[B\x1b[B\x1b[B");
    pty.send("\r");

    let status = wait_exit(&mut pty, &mut child);
    assert!(!status.success(), "abort should exit non-zero");
    assert_eq!(
        std::fs::read_to_string(home.path().join(".legion/legion.json")).unwrap(),
        before,
        "abort must leave the config untouched"
    );
}
