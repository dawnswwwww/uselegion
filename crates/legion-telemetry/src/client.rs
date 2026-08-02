use std::sync::Arc;

use legion_core::config::TelemetryConfig;
use legion_core::fs::expand_tilde;

use crate::{LogEntry, LogLevel, LogSource, SessionMetric, TelemetryError, UnifiedLog};

/// Product telemetry client: unified log + session metrics.
///
/// The client is cheaply cloneable and safe to share across async tasks. It
/// writes to local JSONL files synchronously under a mutex; calls are small
/// enough that this keeps the implementation simple while remaining durable.
#[derive(Clone, Debug, Default)]
pub struct TelemetryClient {
    unified_log: Option<Arc<UnifiedLog>>,
    session_metrics_log: Option<Arc<UnifiedLog>>,
}

impl TelemetryClient {
    /// Build a client from configuration, creating log directories as needed.
    pub fn from_config(config: &TelemetryConfig) -> Result<Self, TelemetryError> {
        if !config.enabled || config.mode == "disabled" {
            return Ok(Self::default());
        }

        let unified_log = if config.unified_log {
            Some(Arc::new(UnifiedLog::new(
                expand_tilde(&config.unified_log_path),
                config.max_log_bytes,
            )?))
        } else {
            None
        };

        let session_metrics_log = if config.session_metrics {
            Some(Arc::new(UnifiedLog::new(
                expand_tilde(&config.session_metrics_path),
                config.max_log_bytes,
            )?))
        } else {
            None
        };

        Ok(Self {
            unified_log,
            session_metrics_log,
        })
    }

    /// Write a generic unified log entry.
    pub async fn log(
        &self,
        level: LogLevel,
        source: LogSource,
        message: impl Into<String>,
        session_id: Option<&str>,
        ctx: Option<serde_json::Value>,
    ) {
        let Some(log) = self.unified_log.as_ref() else {
            return;
        };
        let entry = LogEntry::new(
            source,
            level,
            message.into(),
            session_id.map(|s| s.to_string()),
            ctx,
        );
        if let Err(err) = log.emit(&entry) {
            tracing::warn!(error = %err, "failed to write unified log entry");
        }
    }

    /// Write a session metric event.
    pub async fn log_session_event(&self, event: SessionMetric) {
        let Some(log) = self.session_metrics_log.as_ref() else {
            return;
        };
        match serde_json::to_value(&event) {
            Ok(mut value) => {
                // Stamp every record with its emission time (`ts`, RFC 3339,
                // same convention as the unified log) so event spacing and
                // overlap are reconstructible from the file alone.
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert("ts".to_string(), chrono::Utc::now().to_rfc3339().into());
                }
                if let Err(err) = log.emit_raw(&value) {
                    tracing::warn!(error = %err, "failed to write session metric");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to serialize session metric");
            }
        }
    }

    /// Convenience wrapper for structured session context.
    pub async fn log_info(
        &self,
        source: LogSource,
        message: impl Into<String>,
        session_id: Option<&str>,
    ) {
        self.log(LogLevel::Info, source, message, session_id, None)
            .await;
    }

    /// Return the configured log path for introspection in tests.
    pub fn unified_log_path(&self) -> Option<&std::path::Path> {
        self.unified_log.as_ref().map(|l| l.path())
    }

    /// Return the configured session metrics path for introspection in tests.
    pub fn session_metrics_path(&self) -> Option<&std::path::Path> {
        self.session_metrics_log.as_ref().map(|l| l.path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_core::config::TelemetryConfig;
    use tempfile::TempDir;

    fn config_in(dir: &TempDir) -> TelemetryConfig {
        TelemetryConfig {
            enabled: true,
            mode: "enabled".to_string(),
            unified_log: true,
            session_metrics: true,
            unified_log_path: dir
                .path()
                .join("unified.jsonl")
                .to_string_lossy()
                .to_string(),
            session_metrics_path: dir
                .path()
                .join("metrics.jsonl")
                .to_string_lossy()
                .to_string(),
            max_log_bytes: 5_000_000,
            events_url: None,
            mixpanel_token: None,
        }
    }

    #[tokio::test]
    async fn writes_session_metric_jsonl() {
        let dir = TempDir::new().unwrap();
        let client = TelemetryClient::from_config(&config_in(&dir)).unwrap();
        let event = SessionMetric::SessionStarted {
            session_id: "s1".to_string(),
            agent_id: "agent-1".to_string(),
            model_ref: "openai/gpt-4o".to_string(),
        };
        client.log_session_event(event.clone()).await;

        let contents = std::fs::read_to_string(client.session_metrics_path().unwrap()).unwrap();
        let line = contents.lines().next().unwrap();
        // The event payload round-trips (unknown fields are ignored).
        let parsed: SessionMetric = serde_json::from_str(line).unwrap();
        assert_eq!(parsed, event);
        // Every record is stamped with an RFC 3339 emission time.
        let raw: serde_json::Value = serde_json::from_str(line).unwrap();
        let ts = raw.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "metric record must carry an RFC 3339 ts field, got {ts:?}"
        );
    }

    #[tokio::test]
    async fn disabled_client_does_not_create_files() {
        let dir = TempDir::new().unwrap();
        let mut config = config_in(&dir);
        config.enabled = false;
        let client = TelemetryClient::from_config(&config).unwrap();
        assert!(client.unified_log_path().is_none());
        assert!(client.session_metrics_path().is_none());
    }

    #[tokio::test]
    async fn log_emits_unified_entry() {
        let dir = TempDir::new().unwrap();
        let client = TelemetryClient::from_config(&config_in(&dir)).unwrap();
        client
            .log(
                LogLevel::Info,
                LogSource::Runtime,
                "hello telemetry",
                Some("s1"),
                None,
            )
            .await;

        let contents = std::fs::read_to_string(client.unified_log_path().unwrap()).unwrap();
        let parsed: LogEntry = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.lvl, LogLevel::Info);
        assert_eq!(parsed.src, LogSource::Runtime);
        assert_eq!(parsed.msg, "hello telemetry");
        assert_eq!(parsed.sid, Some("s1".to_string()));
    }
}
