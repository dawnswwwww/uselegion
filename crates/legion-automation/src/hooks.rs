//! Event-driven hooks triggered by lifecycle events.
//!
//! Hooks can be either executable scripts in `~/.legion/hooks/<event-name>.sh`
//! (or `.py`, etc.) or in-process implementations registered by plugins via the
//! [`Hook`] trait.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Lifecycle events that can trigger hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before the agent system prompt is finalized.
    AgentBootstrap,
    /// The `/new` command was issued.
    CommandNew,
    /// The `/reset` command was issued.
    CommandReset,
    /// The `/stop` command was issued.
    CommandStop,
    /// The Gateway is starting.
    GatewayStart,
    /// The Gateway is stopping.
    GatewayStop,
}

impl HookEvent {
    /// Canonical string name used for script lookup and logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::AgentBootstrap => "agent:bootstrap",
            HookEvent::CommandNew => "command:new",
            HookEvent::CommandReset => "command:reset",
            HookEvent::CommandStop => "command:stop",
            HookEvent::GatewayStart => "gateway:start",
            HookEvent::GatewayStop => "gateway:stop",
        }
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Context passed to a hook implementation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookContext {
    /// The event that triggered the hook.
    pub event: String,
    /// Additional event-specific key/value pairs.
    #[serde(flatten)]
    pub extras: HashMap<String, serde_json::Value>,
}

impl HookContext {
    pub fn new(event: HookEvent) -> Self {
        Self {
            event: event.as_str().to_string(),
            extras: HashMap::new(),
        }
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.extras.insert(key.into(), value.into());
        self
    }
}

/// Errors that can occur when running hooks.
#[derive(Debug, Error)]
pub enum HookError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hook '{0}' exited with code {1:?}")]
    ScriptFailed(String, Option<i32>),
    #[error("hook '{0}' not found")]
    NotFound(String),
}

/// In-process hook trait for plugins and tests.
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// The event this hook handles.
    fn event(&self) -> HookEvent;

    /// Run the hook with the provided context.
    async fn run(&self, ctx: &HookContext) -> Result<(), HookError>;
}

/// A hook runner that dispatches to both script and in-process hooks.
pub struct HookRunner {
    hooks_dir: PathBuf,
    in_process: Vec<Arc<dyn Hook>>,
}

impl HookRunner {
    /// Create a runner using the default `~/.legion/hooks` directory.
    pub fn default_dir() -> Self {
        Self::new(default_hooks_dir())
    }

    /// Create a runner with a specific hooks directory.
    pub fn new(hooks_dir: impl Into<PathBuf>) -> Self {
        Self {
            hooks_dir: hooks_dir.into(),
            in_process: Vec::new(),
        }
    }

    /// Register an in-process hook.
    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.in_process.push(hook);
    }

    /// Run all hooks for the given event. Failures are logged but do not stop
    /// other hooks from running.
    pub async fn run(&self, ctx: &HookContext) {
        let event = ctx.event.clone();
        let mut errors = Vec::new();

        for hook in &self.in_process {
            if hook.event().as_str() == event {
                if let Err(err) = hook.run(ctx).await {
                    tracing::warn!(event = %event, error = %err, "in-process hook failed");
                    errors.push(err.to_string());
                }
            }
        }

        if let Err(err) = self.run_scripts(ctx).await {
            tracing::warn!(event = %event, error = %err, "script hook failed");
            errors.push(err.to_string());
        }

        if !errors.is_empty() {
            tracing::debug!(event = %event, errors = ?errors, "hook run completed with errors");
        }
    }

    async fn run_scripts(&self, ctx: &HookContext) -> Result<(), HookError> {
        let event_name = ctx.event.replace(':', "-");
        if !self.hooks_dir.exists() {
            return Ok(());
        }

        let mut entries = tokio::fs::read_dir(&self.hooks_dir).await?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem != event_name {
                continue;
            }
            run_script(&path, ctx).await?;
        }
        Ok(())
    }
}

async fn run_script(path: &Path, ctx: &HookContext) -> Result<(), HookError> {
    let json = serde_json::to_string(ctx)?;
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("LEGION_HOOK_EVENT", &ctx.event);

    for (key, value) in &ctx.extras {
        let var_name = format!("LEGION_HOOK_{}", key.to_uppercase());
        cmd.env(var_name, value.to_string());
    }

    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(json.as_bytes()).await;
    }

    let status = child.wait().await?;
    if !status.success() {
        let code = status.code();
        return Err(HookError::ScriptFailed(path.display().to_string(), code));
    }
    Ok(())
}

fn default_hooks_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".legion").join("hooks"))
        .unwrap_or_else(|| PathBuf::from(".legion/hooks"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Default)]
    struct CollectingHook {
        calls: std::sync::Mutex<Vec<HookContext>>,
    }

    #[async_trait::async_trait]
    impl Hook for CollectingHook {
        fn event(&self) -> HookEvent {
            HookEvent::GatewayStart
        }

        async fn run(&self, ctx: &HookContext) -> Result<(), HookError> {
            self.calls.lock().unwrap().push(ctx.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn should_run_in_process_hook() {
        let hook = Arc::new(CollectingHook::default());
        let mut runner = HookRunner::new("/nonexistent");
        runner.register(hook.clone());

        let ctx = HookContext::new(HookEvent::GatewayStart).with("gateway_id", "gw-1");
        runner.run(&ctx).await;

        let calls = hook.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].event, "gateway:start");
        assert_eq!(calls[0].extras.get("gateway_id").unwrap(), "gw-1");
    }

    #[tokio::test]
    async fn should_skip_hooks_for_other_events() {
        let hook = Arc::new(CollectingHook::default());
        let mut runner = HookRunner::new("/nonexistent");
        runner.register(hook.clone());

        runner.run(&HookContext::new(HookEvent::CommandNew)).await;

        let calls = hook.calls.lock().unwrap();
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn should_run_executable_script_hook() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gateway-start.sh");
        let output = dir.path().join("hook-output.json");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                &script,
                format!("#!/bin/sh\ncat > {}\n", output.to_str().unwrap()),
            )
            .unwrap();
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            std::fs::write(
                &script,
                format!("@echo off\ntype con > \"{}\"\n", output.to_str().unwrap()),
            )
            .unwrap();
        }

        let runner = HookRunner::new(dir.path());
        let ctx = HookContext::new(HookEvent::GatewayStart).with("gateway_id", json!("gw-2"));
        runner.run(&ctx).await;

        let written = std::fs::read_to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["event"], "gateway:start");
        assert_eq!(parsed["gateway_id"], "gw-2");
    }

    /// Write an executable hook script that runs `body` (a shell command).
    fn write_script(path: &Path, body: &str) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, format!("@echo off\n{body}\n")).unwrap();
        }
    }

    struct FailingHook;

    #[async_trait::async_trait]
    impl Hook for FailingHook {
        fn event(&self) -> HookEvent {
            HookEvent::GatewayStart
        }

        async fn run(&self, _ctx: &HookContext) -> Result<(), HookError> {
            Err(HookError::ScriptFailed("failing-hook".to_string(), Some(1)))
        }
    }

    #[tokio::test]
    async fn failing_script_hook_is_swallowed_and_others_still_run() {
        let dir = tempfile::tempdir().unwrap();
        write_script(&dir.path().join("gateway-start.sh"), "exit 1");

        let collecting = Arc::new(CollectingHook::default());
        let mut runner = HookRunner::new(dir.path());
        // A failing in-process hook must not stop the next one, and the
        // failing script must not abort the whole run.
        runner.register(Arc::new(FailingHook));
        runner.register(collecting.clone());

        runner.run(&HookContext::new(HookEvent::GatewayStart)).await;

        let calls = collecting.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "other hooks must still run after failures");
    }

    #[tokio::test]
    async fn should_not_execute_scripts_with_non_matching_stem() {
        let dir = tempfile::tempdir().unwrap();
        let matching_marker = dir.path().join("matching.marker");
        let other_event_marker = dir.path().join("other-event.marker");
        let similar_stem_marker = dir.path().join("similar-stem.marker");

        write_script(
            &dir.path().join("gateway-start.sh"),
            &format!("touch {}", matching_marker.display()),
        );
        // Different event: stem is "command-new", not "gateway-start".
        write_script(
            &dir.path().join("command-new.sh"),
            &format!("touch {}", other_event_marker.display()),
        );
        // Similar but not equal stem ("gateway-start-old").
        write_script(
            &dir.path().join("gateway-start-old.sh"),
            &format!("touch {}", similar_stem_marker.display()),
        );

        let runner = HookRunner::new(dir.path());
        runner.run(&HookContext::new(HookEvent::GatewayStart)).await;

        assert!(
            matching_marker.exists(),
            "control script for the fired event must run"
        );
        assert!(
            !other_event_marker.exists(),
            "scripts for other events must not run"
        );
        assert!(
            !similar_stem_marker.exists(),
            "scripts with a non-matching stem must not run"
        );
    }
}
