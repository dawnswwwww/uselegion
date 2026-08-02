use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::TelemetryError;
use legion_core::fs::expand_tilde;

/// Source component that emitted a log entry.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    Shell,
    Cli,
    Gateway,
    Channel,
    Runtime,
}

/// Severity level of a log entry.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// A single JSONL record in the unified log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub ts: String,
    pub src: LogSource,
    pub pid: u32,
    pub ver: Option<String>,
    pub lvl: LogLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx: Option<serde_json::Value>,
}

impl LogEntry {
    pub fn new(
        source: LogSource,
        level: LogLevel,
        message: impl Into<String>,
        session_id: Option<String>,
        ctx: Option<serde_json::Value>,
    ) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339(),
            src: source,
            pid: std::process::id(),
            ver: None,
            lvl: level,
            sid: session_id,
            msg: message.into(),
            ctx,
        }
    }
}

/// Append-only JSONL log with size-based rotation.
///
/// When the log file reaches `max_bytes`, it is renamed to `<path>.1` and a
/// fresh file is created. Only one backup is kept so the total on-disk size
/// stays bounded near `2 * max_bytes`.
pub struct UnifiedLog {
    path: PathBuf,
    max_bytes: u64,
    writer: Mutex<BufWriter<File>>,
}

impl UnifiedLog {
    /// Open (or create) the log file at the expanded path.
    pub fn new(path: impl AsRef<Path>, max_bytes: u64) -> Result<Self, TelemetryError> {
        let path = path.as_ref();
        let path = path
            .to_str()
            .map(expand_tilde)
            .unwrap_or_else(|| path.to_path_buf());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            max_bytes,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Append a single JSONL record, rotating first if the size limit is hit.
    pub fn emit(&self, entry: &LogEntry) -> Result<(), TelemetryError> {
        self.emit_raw(&serde_json::to_value(entry)?)
    }

    /// Append an arbitrary JSON value as a JSONL record.
    pub fn emit_raw(&self, value: &serde_json::Value) -> Result<(), TelemetryError> {
        self.rotate_if_needed()?;
        let mut writer = self.writer.lock().unwrap();
        serde_json::to_writer(&mut *writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    /// Current on-disk log path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate_if_needed(&self) -> Result<(), TelemetryError> {
        let current_size = match std::fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        if current_size >= self.max_bytes {
            let backup = self.path.with_extension(format!(
                "{}.{}",
                self.path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jsonl"),
                1
            ));
            let _ = std::fs::remove_file(&backup);
            std::fs::rename(&self.path, &backup)?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            let mut writer = self.writer.lock().unwrap();
            *writer = BufWriter::new(file);
        }
        Ok(())
    }
}

impl std::fmt::Debug for UnifiedLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedLog")
            .field("path", &self.path)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

/// Convenience handle that can be cheaply cloned into different tasks.
pub type SharedUnifiedLog = Arc<UnifiedLog>;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn emits_jsonl_record() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("unified.jsonl");
        let log = UnifiedLog::new(&path, 5_000_000).unwrap();
        log.emit(&LogEntry::new(
            LogSource::Runtime,
            LogLevel::Info,
            "hello",
            Some("s1".into()),
            None,
        ))
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: LogEntry = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.src, LogSource::Runtime);
        assert_eq!(parsed.lvl, LogLevel::Info);
        assert_eq!(parsed.msg, "hello");
        assert_eq!(parsed.sid, Some("s1".to_string()));
    }

    #[test]
    fn rotates_when_size_exceeded() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("unified.jsonl");
        let log = UnifiedLog::new(&path, 128).unwrap();
        let big = LogEntry::new(LogSource::Cli, LogLevel::Info, "x".repeat(200), None, None);
        log.emit(&big).unwrap();
        assert!(path.exists());

        let backup = path.with_extension("jsonl.1");
        log.emit(&big).unwrap();
        assert!(backup.exists());
        let current_len = std::fs::metadata(&path).unwrap().len();
        assert!(current_len > 0);
    }
}
