//! Shell escape mode for the interactive TUI.
//!
//! In the TUI input box, typing `!` while the input is empty toggles shell
//! mode; the input box title changes to "shell mode". Anything typed in that
//! mode is treated as a shell command and, on Enter, runs locally through the
//! user's shell (`$SHELL`, falling back to `bash`/`sh`). The output is shown
//! as a system message in the chat and is *not* sent to the agent runtime.

/// Run a shell command locally and return a formatted result string.
///
/// The command is executed through the user's `$SHELL` (falling back to
/// `bash` and then `sh`) with the `-c` flag. Output is captured; stdin is
/// not connected, so interactive prompts will fail. Very long output is
/// truncated to keep the TUI responsive.
pub async fn run_shell_command(command: &str) -> String {
    let shell = shell_binary();
    match tokio::process::Command::new(&shell)
        .arg("-c")
        .arg(command)
        .output()
        .await
    {
        Ok(output) => format_output(&output),
        Err(err) => format!("failed to run shell command: {err}"),
    }
}

fn shell_binary() -> String {
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.is_empty() {
            return shell;
        }
    }
    if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

/// Maximum characters of combined stdout/stderr to display in the TUI.
const MAX_OUTPUT_LEN: usize = 8192;

fn format_output(output: &std::process::Output) -> String {
    let mut parts = Vec::new();
    if !output.status.success() {
        parts.push(format!("exit code: {}", output.status.code().unwrap_or(-1)));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        parts.push(stdout.to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        parts.push(format!("stderr: {stderr}"));
    }

    if parts.is_empty() {
        return "(no output)".to_string();
    }

    let mut text = parts.join("\n").trim_end().to_string();
    if text.len() > MAX_OUTPUT_LEN {
        let truncated = &text[..MAX_OUTPUT_LEN];
        let newline = truncated.rfind('\n').unwrap_or(MAX_OUTPUT_LEN);
        text = format!(
            "{}\n[output truncated; {} bytes total]",
            &truncated[..newline],
            text.len()
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_shell_command_captures_stdout() {
        let out = run_shell_command("echo hello world").await;
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn run_shell_command_reports_nonzero_exit() {
        let out = run_shell_command("exit 42").await;
        assert!(out.contains("exit code: 42"), "got: {out}");
    }

    #[tokio::test]
    async fn run_shell_command_captures_stderr() {
        let out = run_shell_command("echo err >&2").await;
        assert!(out.contains("stderr: err"), "got: {out}");
    }

    #[test]
    fn format_output_truncates_very_long_output() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: vec![b'x'; MAX_OUTPUT_LEN + 1000],
            stderr: vec![],
        };
        let text = format_output(&output);
        assert!(text.contains("[output truncated"));
        assert!(text.len() <= MAX_OUTPUT_LEN + 100);
    }
}
