//! Persistent, append-only session transcript store.
//!
//! Each session is stored as a JSONL file under:
//!   `<base_dir>/agents/<agent_id>/sessions/<peer_id>.jsonl`
//!
//! The session key format is:
//!   `agent:<agent_id>:<scope>:<channel>:<account_id>:<peer_kind>:<peer_id>`

pub mod repair;

use crate::session_tools::is_safe_peer_id;
use legion_provider::types::{ChatMessage, ChatRole};
use legion_runtime::types::BoundaryMark;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tracing::{error, warn};

/// On-disk store for conversation transcripts.
#[derive(Debug, Clone)]
pub struct SessionStore {
    base_dir: PathBuf,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            base_dir: dirs::home_dir()
                .map(|h| h.join(".legion"))
                .unwrap_or_else(|| PathBuf::from(".legion")),
        }
    }
}

impl SessionStore {
    /// Create a store rooted at the given directory.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Load all messages for a session key.
    pub async fn load(&self, session_key: &str) -> Vec<ChatMessage> {
        let path = match self.session_path(session_key) {
            Some(p) => p,
            None => return Vec::new(),
        };

        match load_entries(&path).await {
            Ok(entries) => entries.into_iter().filter_map(|e| e.message).collect(),
            Err(err) => {
                error!(path = %path.display(), error = %err, "failed to load session transcript");
                Vec::new()
            }
        }
    }

    /// Load the resumable history for a session key (session-resume Phase A).
    ///
    /// When the transcript contains compaction boundary markers, only
    /// messages written after the **last** boundary are returned: the gateway
    /// persists the compacted head (summary + kept tail) right after each
    /// boundary, so the tail alone is the effective context. Transcripts
    /// without any boundary fall back to a full load (legacy behavior).
    pub async fn load_for_resume(&self, session_key: &str) -> Vec<ChatMessage> {
        let path = match self.session_path(session_key) {
            Some(p) => p,
            None => return Vec::new(),
        };

        match load_entries(&path).await {
            Ok(entries) => {
                let start = entries
                    .iter()
                    .rposition(|e| e.boundary.is_some())
                    .map(|i| i + 1)
                    .unwrap_or(0);
                entries
                    .into_iter()
                    .skip(start)
                    .filter_map(|e| e.message)
                    .collect()
            }
            Err(err) => {
                error!(path = %path.display(), error = %err, "failed to load session transcript");
                Vec::new()
            }
        }
    }

    /// Append messages to a session transcript.
    pub async fn append(&self, session_key: &str, messages: &[ChatMessage]) {
        if messages.is_empty() {
            return;
        }

        let path = match self.session_path(session_key) {
            Some(p) => p,
            None => {
                warn!(session_key, "invalid session key; cannot append transcript");
                return;
            }
        };

        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent).await {
                error!(path = %parent.display(), error = %err, "failed to create session directory");
                return;
            }
        }

        let now = unix_seconds();
        let mut lines = String::new();
        for msg in messages {
            let entry = TranscriptEntry {
                message: Some(msg.clone()),
                boundary: None,
                timestamp: now,
            };
            match serde_json::to_string(&entry) {
                Ok(json) => {
                    lines.push_str(&json);
                    lines.push('\n');
                }
                Err(err) => {
                    error!(error = %err, "failed to serialize transcript entry");
                }
            }
        }

        if lines.is_empty() {
            return;
        }

        Self::write_lines(&path, &lines).await;
    }

    /// Append a compaction boundary marker to a session transcript.
    ///
    /// The marker is written as a boundary-only JSONL entry. The `entry_index`
    /// field is updated to reflect the line number at which the boundary is
    /// written so that session-resume can locate compacted regions.
    pub async fn append_boundary(&self, session_key: &str, boundary: &BoundaryMark) {
        let path = match self.session_path(session_key) {
            Some(p) => p,
            None => {
                warn!(session_key, "invalid session key; cannot append boundary");
                return;
            }
        };

        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent).await {
                error!(path = %parent.display(), error = %err, "failed to create session directory");
                return;
            }
        }

        let entry_index = count_existing_lines(&path).await;
        let mut mark = boundary.clone();
        mark.entry_index = entry_index;
        let entry = TranscriptEntry {
            message: None,
            boundary: Some(mark),
            timestamp: unix_seconds(),
        };

        match serde_json::to_string(&entry) {
            Ok(mut json) => {
                json.push('\n');
                Self::write_lines(&path, &json).await;
            }
            Err(err) => {
                error!(error = %err, "failed to serialize boundary entry");
            }
        }
    }

    async fn write_lines(path: &Path, lines: &str) {
        match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
        {
            Ok(mut file) => {
                if let Err(err) =
                    tokio::io::AsyncWriteExt::write_all(&mut file, lines.as_bytes()).await
                {
                    error!(path = %path.display(), error = %err, "failed to append to session transcript");
                }
            }
            Err(err) => {
                error!(path = %path.display(), error = %err, "failed to open session transcript");
            }
        }
    }

    /// List peer ids that have stored sessions for an agent.
    pub async fn list_sessions(&self, agent_id: &str) -> Vec<String> {
        let dir = self.base_dir.join("agents").join(agent_id).join("sessions");
        let mut entries = Vec::new();
        let mut reader = match fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => return entries,
        };
        while let Ok(Some(entry)) = reader.next_entry().await {
            let name = entry.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if let Some(peer_id) = name_str.strip_suffix(".jsonl") {
                entries.push(peer_id.to_string());
            }
        }
        entries
    }

    /// Lite-read a session transcript: scan only the head of the file
    /// (`buffer_bytes`, default 64 KiB) for the first user prompt instead of
    /// parsing the whole JSONL (session-resume Phase C).
    pub async fn lite_read(
        &self,
        agent_id: &str,
        peer_id: &str,
        buffer_bytes: usize,
    ) -> Option<SessionSummary> {
        let path = self
            .base_dir
            .join("agents")
            .join(agent_id)
            .join("sessions")
            .join(format!("{peer_id}.jsonl"));
        let mut file = fs::File::open(&path).await.ok()?;
        let mut buf = vec![0u8; buffer_bytes.max(1)];
        let n = tokio::io::AsyncReadExt::read(&mut file, &mut buf)
            .await
            .ok()?;
        buf.truncate(n);
        let truncated = file
            .metadata()
            .await
            .map(|m| m.len() as usize > n)
            .unwrap_or(false);
        let text = String::from_utf8_lossy(&buf);
        let first_prompt = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .find_map(|line| {
                let entry: TranscriptEntry = serde_json::from_str(line).ok()?;
                let msg = entry.message?;
                if msg.role == ChatRole::User {
                    Some(msg.content.chars().take(200).collect())
                } else {
                    None
                }
            });
        Some(SessionSummary {
            peer_id: peer_id.to_string(),
            first_prompt,
            truncated,
        })
    }

    /// Lite summaries for every stored session of an agent.
    pub async fn list_session_summaries(
        &self,
        agent_id: &str,
        buffer_bytes: usize,
    ) -> Vec<SessionSummary> {
        let mut out = Vec::new();
        for peer in self.list_sessions(agent_id).await {
            if let Some(summary) = self.lite_read(agent_id, &peer, buffer_bytes).await {
                out.push(summary);
            }
        }
        out
    }

    /// Aggregate statistics for a session transcript (tools-p1p2 Phase A).
    ///
    /// Returns `None` when the session key is invalid or the transcript file
    /// does not exist.
    pub async fn stats(&self, session_key: &str) -> Option<SessionStats> {
        let path = self.session_path(session_key)?;
        let meta = fs::metadata(&path).await.ok()?;
        let entries = match load_entries(&path).await {
            Ok(entries) => entries,
            Err(err) => {
                error!(path = %path.display(), error = %err, "failed to load session transcript");
                return None;
            }
        };

        let mut stats = SessionStats {
            file_bytes: meta.len(),
            ..SessionStats::default()
        };
        for entry in &entries {
            stats.entries += 1;
            if entry.boundary.is_some() {
                stats.boundary_marks += 1;
            }
            if let Some(msg) = &entry.message {
                match msg.role {
                    ChatRole::User => stats.user_messages += 1,
                    ChatRole::Assistant => stats.assistant_messages += 1,
                    ChatRole::Tool => stats.tool_messages += 1,
                    ChatRole::System => stats.system_messages += 1,
                }
            }
        }
        stats.last_timestamp = entries.last().map(|e| e.timestamp);
        Some(stats)
    }

    /// Read all messages from an agent/peer transcript.
    ///
    /// Returns `None` when the transcript file does not exist or cannot be
    /// read; corrupt lines are skipped like [`SessionStore::load`].
    pub async fn transcript_messages(
        &self,
        agent_id: &str,
        peer_id: &str,
    ) -> Option<Vec<ChatMessage>> {
        let path = self
            .base_dir
            .join("agents")
            .join(agent_id)
            .join("sessions")
            .join(format!("{peer_id}.jsonl"));
        fs::metadata(&path).await.ok()?;
        match load_entries(&path).await {
            Ok(entries) => Some(entries.into_iter().filter_map(|e| e.message).collect()),
            Err(err) => {
                error!(path = %path.display(), error = %err, "failed to load session transcript");
                None
            }
        }
    }

    /// Archive transcripts whose last entry is older than `ttl_days`
    /// (session-resume Phase C). Files are **moved**, not deleted, to
    /// `<archive_dir>/agents/<agent>/sessions/<peer>.jsonl` — restore by
    /// moving them back. `ttl_days == 0` is a no-op. Returns the archived
    /// destination paths.
    pub async fn archive_expired(&self, ttl_days: u64, archive_dir: &Path) -> Vec<PathBuf> {
        let mut archived = Vec::new();
        if ttl_days == 0 {
            return archived;
        }
        let cutoff = unix_seconds().saturating_sub(ttl_days.saturating_mul(86_400));
        let agents_dir = self.base_dir.join("agents");
        let mut agents = match fs::read_dir(&agents_dir).await {
            Ok(r) => r,
            Err(_) => return archived,
        };
        while let Ok(Some(agent_entry)) = agents.next_entry().await {
            let agent_name = agent_entry.file_name();
            let sessions_dir = agent_entry.path().join("sessions");
            let mut files = match fs::read_dir(&sessions_dir).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(file_entry)) = files.next_entry().await {
                let path = file_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(last_ts) = last_entry_timestamp(&path).await else {
                    continue;
                };
                if last_ts >= cutoff {
                    continue;
                }
                let dest_dir = archive_dir
                    .join("agents")
                    .join(&agent_name)
                    .join("sessions");
                if let Err(err) = fs::create_dir_all(&dest_dir).await {
                    error!(path = %dest_dir.display(), error = %err, "failed to create archive directory");
                    continue;
                }
                let dest = dest_dir.join(file_entry.file_name());
                match fs::rename(&path, &dest).await {
                    Ok(()) => {
                        warn!(
                            from = %path.display(),
                            to = %dest.display(),
                            "archived expired session transcript"
                        );
                        archived.push(dest);
                    }
                    Err(err) => {
                        error!(path = %path.display(), error = %err, "failed to archive session transcript");
                    }
                }
            }
        }
        archived
    }

    /// Resolve a session key to its on-disk path.
    ///
    /// Both the agent id and the peer id land directly on the filesystem
    /// (`agents/<agent>/sessions/<peer>.jsonl`), so keys carrying path
    /// separators or other special characters are rejected outright.
    fn session_path(&self, session_key: &str) -> Option<PathBuf> {
        let parts: Vec<&str> = session_key.split(':').collect();
        if parts.len() != 7 || parts[0] != "agent" {
            return None;
        }
        let agent_id = parts[1];
        let peer_id = parts[6];
        if !is_safe_peer_id(agent_id) || !is_safe_peer_id(peer_id) {
            return None;
        }
        Some(
            self.base_dir
                .join("agents")
                .join(agent_id)
                .join("sessions")
                .join(format!("{}.jsonl", peer_id)),
        )
    }
}

async fn load_entries(path: &Path) -> io::Result<Vec<TranscriptEntry>> {
    let text = match fs::read_to_string(path).await {
        Ok(t) => t,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut entries = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TranscriptEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                warn!(line = line_no + 1, error = %err, "skipping corrupt transcript line");
            }
        }
    }
    Ok(entries)
}

async fn count_existing_lines(path: &Path) -> usize {
    match fs::read_to_string(path).await {
        Ok(text) => text.lines().count(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
        Err(err) => {
            error!(path = %path.display(), error = %err, "failed to count transcript lines");
            0
        }
    }
}

/// Timestamp of the last parseable transcript entry, read from the file tail
/// only (last 8 KiB) so archiving does not parse whole transcripts.
async fn last_entry_timestamp(path: &Path) -> Option<u64> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    let start = len.saturating_sub(8192);
    file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find_map(|line| serde_json::from_str::<TranscriptEntry>(line).ok())
        .map(|e| e.timestamp)
}

/// Aggregate statistics for a session transcript (tools-p1p2 Phase A).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Total number of transcript entries (messages + boundary markers).
    pub entries: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_messages: usize,
    pub system_messages: usize,
    /// Number of compaction boundary markers in the transcript.
    pub boundary_marks: usize,
    /// Timestamp (unix seconds) of the last transcript entry.
    pub last_timestamp: Option<u64>,
    /// Size of the transcript file in bytes.
    pub file_bytes: u64,
}

/// Lite summary of a session transcript (head-only read).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    pub peer_id: String,
    /// First user prompt in the transcript (truncated to 200 chars).
    pub first_prompt: Option<String>,
    /// Whether the transcript is larger than the lite-read buffer, so the
    /// summary may be incomplete.
    pub truncated: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct TranscriptEntry {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    message: Option<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    boundary: Option<BoundaryMark>,
    timestamp: u64,
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_provider::types::{ChatRole, FunctionCall, ToolCall};
    use tempfile::TempDir;

    fn store() -> (SessionStore, TempDir) {
        let dir = TempDir::new().unwrap();
        (SessionStore::new(dir.path()), dir)
    }

    fn session_key(agent_id: &str, peer_id: &str) -> String {
        format!("agent:{}:dm:webchat:default:direct:{}", agent_id, peer_id)
    }

    #[tokio::test]
    async fn loads_empty_for_missing_session() {
        let (store, _dir) = store();
        let msgs = store.load(&session_key("main", "user1")).await;
        assert!(msgs.is_empty());
    }

    #[test]
    fn session_path_resolves_valid_key() {
        let (store, dir) = store();
        let path = store.session_path(&session_key("main", "user1")).unwrap();
        assert_eq!(path, dir.path().join("agents/main/sessions/user1.jsonl"));
    }

    #[test]
    fn session_path_rejects_traversal_peer_id() {
        let (store, _dir) = store();
        for bad in ["../../etc/evil", "a/b", "..\\x", ""] {
            let key = session_key("main", bad);
            assert!(
                store.session_path(&key).is_none(),
                "peer id {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn session_path_rejects_unsafe_agent_id() {
        let (store, _dir) = store();
        for bad in ["../../etc/evil", "a/b", "..\\x", ""] {
            let key = session_key(bad, "user1");
            assert!(
                store.session_path(&key).is_none(),
                "agent id {bad:?} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn lite_read_missing_file_returns_none() {
        let (store, _dir) = store();
        assert!(store.lite_read("main", "ghost", 65_536).await.is_none());
    }

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store
            .append(
                &key,
                &[ChatMessage::user("hi"), ChatMessage::assistant("hello")],
            )
            .await;

        let msgs = store.load(&key).await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
        assert_eq!(msgs[1].content, "hello");
    }

    #[tokio::test]
    async fn append_is_append_only() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store.append(&key, &[ChatMessage::user("first")]).await;
        store
            .append(&key, &[ChatMessage::assistant("second")])
            .await;

        let msgs = store.load(&key).await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "first");
        assert_eq!(msgs[1].content, "second");
    }

    #[tokio::test]
    async fn persists_tool_calls() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        let mut msg = ChatMessage::assistant("");
        msg.tool_calls = Some(vec![ToolCall {
            id: "call-1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: r#"{"path":"x"}"#.into(),
            },
        }]);
        store.append(&key, &[msg]).await;

        let msgs = store.load(&key).await;
        assert_eq!(msgs.len(), 1);
        let tc = msgs[0].tool_calls.as_ref().unwrap().first().unwrap();
        assert_eq!(tc.id, "call-1");
        assert_eq!(tc.function.name, "read");
    }

    #[tokio::test]
    async fn skips_corrupt_lines() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        let path = store.session_path(&key).unwrap();
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).await.unwrap();
        fs::write(&path, "{not json}\n{}\n").await.unwrap();

        let msgs = store.load(&key).await;
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn list_sessions_returns_peer_ids() {
        let (store, _dir) = store();
        store
            .append(&session_key("main", "alice"), &[ChatMessage::user("hi")])
            .await;
        store
            .append(&session_key("main", "bob"), &[ChatMessage::user("hi")])
            .await;

        let peers = store.list_sessions("main").await;
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&"alice".to_string()));
        assert!(peers.contains(&"bob".to_string()));
    }

    #[tokio::test]
    async fn append_boundary_persists_marker_and_keeps_messages_loadable() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store.append(&key, &[ChatMessage::user("hi")]).await;

        let boundary = BoundaryMark {
            entry_index: 0,
            timestamp_iso: "2026-07-09T12:00:00.000Z".to_string(),
            tokens_compacted: 123,
        };
        store.append_boundary(&key, &boundary).await;
        store.append(&key, &[ChatMessage::assistant("hello")]).await;

        // Messages on either side of the boundary remain loadable.
        let msgs = store.load(&key).await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[1].content, "hello");

        // The boundary line was written with the correct entry index.
        let path = store.session_path(&key).unwrap();
        let text = fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        let boundary_line: TranscriptEntry = serde_json::from_str(lines[1]).unwrap();
        assert!(boundary_line.message.is_none());
        let mark = boundary_line.boundary.unwrap();
        assert_eq!(mark.entry_index, 1);
        assert_eq!(mark.tokens_compacted, 123);
    }

    fn boundary_mark() -> BoundaryMark {
        BoundaryMark {
            entry_index: 0,
            timestamp_iso: "2026-07-11T12:00:00.000Z".to_string(),
            tokens_compacted: 100,
        }
    }

    #[tokio::test]
    async fn load_for_resume_returns_only_post_boundary_messages() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store
            .append(&key, &[ChatMessage::user("old question")])
            .await;
        store.append_boundary(&key, &boundary_mark()).await;
        // The gateway persists the compacted head right after the boundary.
        store
            .append(
                &key,
                &[
                    ChatMessage::system("Earlier conversation summary:\n\ntalked about X"),
                    ChatMessage::user("old question"),
                    ChatMessage::assistant("new answer"),
                ],
            )
            .await;

        let resumed = store.load_for_resume(&key).await;
        assert_eq!(resumed.len(), 3);
        assert_eq!(resumed[0].role, ChatRole::System);
        assert!(resumed[0].content.contains("summary"));
        assert_eq!(resumed[2].content, "new answer");

        // Full load still returns everything (raw messages, boundary skipped).
        let full = store.load(&key).await;
        assert_eq!(full.len(), 4);
    }

    #[tokio::test]
    async fn load_for_resume_without_boundary_loads_all() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store
            .append(
                &key,
                &[ChatMessage::user("hi"), ChatMessage::assistant("hello")],
            )
            .await;

        let resumed = store.load_for_resume(&key).await;
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0].content, "hi");
    }

    #[tokio::test]
    async fn load_for_resume_uses_last_boundary() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store.append(&key, &[ChatMessage::user("first")]).await;
        store.append_boundary(&key, &boundary_mark()).await;
        store
            .append(&key, &[ChatMessage::system("summary one")])
            .await;
        store.append_boundary(&key, &boundary_mark()).await;
        store
            .append(
                &key,
                &[
                    ChatMessage::system("summary two"),
                    ChatMessage::user("after second"),
                ],
            )
            .await;

        let resumed = store.load_for_resume(&key).await;
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0].content, "summary two");
        assert_eq!(resumed[1].content, "after second");
    }

    #[tokio::test]
    async fn load_for_resume_skips_corrupt_lines_around_boundary() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store.append(&key, &[ChatMessage::user("hi")]).await;
        store.append_boundary(&key, &boundary_mark()).await;
        let path = store.session_path(&key).unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        use tokio::io::AsyncWriteExt;
        file.write_all(b"{corrupt\n").await.unwrap();
        drop(file);
        store.append(&key, &[ChatMessage::assistant("kept")]).await;

        let resumed = store.load_for_resume(&key).await;
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].content, "kept");
    }

    #[tokio::test]
    async fn lite_read_extracts_first_prompt() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store
            .append(
                &key,
                &[
                    ChatMessage::user("first prompt here"),
                    ChatMessage::assistant("reply"),
                    ChatMessage::user("second prompt"),
                ],
            )
            .await;

        let summary = store.lite_read("main", "user1", 65_536).await.unwrap();
        assert_eq!(summary.peer_id, "user1");
        assert_eq!(summary.first_prompt.as_deref(), Some("first prompt here"));
        assert!(!summary.truncated);
    }

    #[tokio::test]
    async fn lite_read_marks_truncated_when_file_exceeds_buffer() {
        let (store, _dir) = store();
        let key = session_key("main", "user1");
        store
            .append(
                &key,
                &[
                    ChatMessage::user("x".repeat(500)),
                    ChatMessage::assistant("y".repeat(500)),
                ],
            )
            .await;

        // Buffer covers the first JSONL line but not the whole file.
        let summary = store.lite_read("main", "user1", 600).await.unwrap();
        assert!(summary.truncated);
        // Head-only read still sees the first prompt (capped at 200 chars).
        assert_eq!(summary.first_prompt.as_ref().map(|s| s.len()), Some(200));
    }

    #[tokio::test]
    async fn list_session_summaries_covers_all_peers() {
        let (store, _dir) = store();
        store
            .append(
                &session_key("main", "alice"),
                &[ChatMessage::user("hi alice")],
            )
            .await;
        store
            .append(&session_key("main", "bob"), &[ChatMessage::user("hi bob")])
            .await;

        let summaries = store.list_session_summaries("main", 65_536).await;
        assert_eq!(summaries.len(), 2);
        assert!(
            summaries
                .iter()
                .any(|s| s.first_prompt.as_deref() == Some("hi alice"))
        );
        assert!(
            summaries
                .iter()
                .any(|s| s.first_prompt.as_deref() == Some("hi bob"))
        );
    }

    #[tokio::test]
    async fn archive_expired_moves_old_transcripts_and_keeps_recent() {
        let (store, dir) = store();
        let old_key = session_key("main", "old-peer");
        let new_key = session_key("main", "new-peer");
        store
            .append(&old_key, &[ChatMessage::user("ancient")])
            .await;
        store.append(&new_key, &[ChatMessage::user("fresh")]).await;

        // Backdate the old transcript's entry timestamp to 10 days ago.
        let old_path = store.session_path(&old_key).unwrap();
        let text = fs::read_to_string(&old_path).await.unwrap();
        let mut entry: TranscriptEntry = serde_json::from_str(text.trim()).unwrap();
        entry.timestamp = unix_seconds() - 10 * 86_400;
        fs::write(
            &old_path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .await
        .unwrap();

        let archive_dir = dir.path().join("archive");
        let archived = store.archive_expired(7, &archive_dir).await;
        assert_eq!(archived.len(), 1);
        assert!(archived[0].ends_with("agents/main/sessions/old-peer.jsonl"));
        assert!(
            fs::metadata(&archived[0]).await.is_ok(),
            "file moved to archive"
        );
        assert!(
            fs::metadata(&old_path).await.is_err(),
            "old transcript removed from live store"
        );
        assert!(
            fs::metadata(store.session_path(&new_key).unwrap())
                .await
                .is_ok(),
            "recent transcript untouched"
        );

        // Restoration is just moving the file back.
        fs::create_dir_all(old_path.parent().unwrap())
            .await
            .unwrap();
        fs::rename(&archived[0], &old_path).await.unwrap();
        let msgs = store.load(&old_key).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "ancient");
    }

    #[tokio::test]
    async fn archive_expired_zero_ttl_is_noop() {
        let (store, dir) = store();
        let key = session_key("main", "user1");
        store.append(&key, &[ChatMessage::user("hi")]).await;
        let archived = store.archive_expired(0, &dir.path().join("archive")).await;
        assert!(archived.is_empty());
        assert!(
            fs::metadata(store.session_path(&key).unwrap())
                .await
                .is_ok()
        );
    }
}
