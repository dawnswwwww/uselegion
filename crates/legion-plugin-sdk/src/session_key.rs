//! Session key format — the single source of truth.
//!
//! A session key identifies one conversation transcript and has exactly seven
//! `:`-separated segments:
//!
//! ```text
//! agent:<agent_id>:<scope>:<channel>:<account_id>:<peer_kind>:<peer_id>
//! ```
//!
//! - `scope` is a free-form routing scope (`dm`, `main`, `cron`, `subagent`,
//!   `flow`, `a2a`, …).
//! - `channel` / `account_id` identify the originating channel account; local
//!   producers use placeholders such as `cli`, `spawn`, or `default`.
//! - `peer_kind` is one of `direct` / `group` / `thread` ([`PeerKind`]).
//! - `agent_id` and `peer_id` land directly on the filesystem
//!   (`agents/<agent>/sessions/<peer>.jsonl`), so they must satisfy
//!   [`is_safe_segment`] for the session to be persistable.

use crate::channel::PeerKind;

/// Number of `:`-separated segments in a session key.
pub const SESSION_KEY_SEGMENTS: usize = 7;

/// Parsed components of a session key.
#[derive(Debug, Clone)]
pub struct SessionKeyParts {
    pub agent_id: String,
    pub scope: String,
    pub channel: String,
    pub account_id: String,
    pub peer_kind: PeerKind,
    pub peer_id: String,
}

impl SessionKeyParts {
    pub fn new(
        agent_id: impl Into<String>,
        scope: impl Into<String>,
        channel: impl Into<String>,
        account_id: impl Into<String>,
        peer_kind: PeerKind,
        peer_id: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            scope: scope.into(),
            channel: channel.into(),
            account_id: account_id.into(),
            peer_kind,
            peer_id: peer_id.into(),
        }
    }

    /// Parts for a direct-peer session (the common case for local,
    /// non-channel sessions such as CLI, cron, sub-agent, or a2a runs).
    pub fn direct(
        agent_id: impl Into<String>,
        scope: impl Into<String>,
        channel: impl Into<String>,
        account_id: impl Into<String>,
        peer_id: impl Into<String>,
    ) -> Self {
        Self::new(
            agent_id,
            scope,
            channel,
            account_id,
            PeerKind::Direct,
            peer_id,
        )
    }

    /// Render the parts back into a session key using their own agent id.
    pub fn key(&self) -> String {
        build_session_key(&self.agent_id.clone(), self)
    }
}

/// Parse a session key into its components; `None` when the key does not
/// have the canonical seven-segment shape.
pub fn parse_session_key(key: &str) -> Option<SessionKeyParts> {
    let parts: Vec<&str> = key.split(':').collect();
    if parts.len() != SESSION_KEY_SEGMENTS || parts[0] != "agent" {
        return None;
    }
    let peer_kind = match parts[5] {
        "direct" => PeerKind::Direct,
        "group" => PeerKind::Group,
        "thread" => PeerKind::Thread,
        _ => return None,
    };
    Some(SessionKeyParts {
        agent_id: parts[1].to_string(),
        scope: parts[2].to_string(),
        channel: parts[3].to_string(),
        account_id: parts[4].to_string(),
        peer_kind,
        peer_id: parts[6].to_string(),
    })
}

/// Build a session key from `parts`, overriding the agent id (routing
/// re-resolves the agent from bindings and rebuilds the key with it).
pub fn build_session_key(agent_id: &str, parts: &SessionKeyParts) -> String {
    let peer_kind = match parts.peer_kind {
        PeerKind::Direct => "direct",
        PeerKind::Group => "group",
        PeerKind::Thread => "thread",
    };
    format!(
        "agent:{}:{}:{}:{}:{}:{}",
        agent_id, parts.scope, parts.channel, parts.account_id, peer_kind, parts.peer_id
    )
}

/// Build a direct-peer session key. This is the canonical constructor for
/// every local producer (CLI, TUI, cron, heartbeat, tasks, sub-agents, a2a).
pub fn direct_session_key(
    agent_id: &str,
    scope: &str,
    channel: &str,
    account_id: &str,
    peer_id: &str,
) -> String {
    SessionKeyParts::direct(agent_id, scope, channel, account_id, peer_id).key()
}

/// A segment that is safe to use as a file name component: non-empty ASCII
/// alphanumerics plus `.`, `_`, `-` (which also excludes path separators).
pub fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_build_round_trip() {
        let key = "agent:main:dm:telegram:default:group:chat-42";
        let parts = parse_session_key(key).expect("valid key parses");
        assert_eq!(parts.agent_id, "main");
        assert_eq!(parts.scope, "dm");
        assert_eq!(parts.channel, "telegram");
        assert_eq!(parts.account_id, "default");
        assert_eq!(parts.peer_kind, PeerKind::Group);
        assert_eq!(parts.peer_id, "chat-42");
        assert_eq!(parts.key(), key);
        assert_eq!(
            build_session_key("work", &parts),
            "agent:work:dm:telegram:default:group:chat-42"
        );
    }

    #[test]
    fn parse_rejects_non_canonical_shapes() {
        assert!(parse_session_key("bogus").is_none());
        assert!(parse_session_key("agent:main:dm:cli:default:peer:cli").is_none());
        // Former flow/a2a shapes (5 and 4 segments) are not valid keys.
        assert!(parse_session_key("agent:main:flow:f1:step1").is_none());
        assert!(parse_session_key("agent:main:a2a:helper").is_none());
    }

    #[test]
    fn direct_session_key_builds_seven_segments() {
        let key = direct_session_key("main", "cron", "cron", "default", "job-1");
        assert_eq!(key, "agent:main:cron:cron:default:direct:job-1");
        assert_eq!(key.split(':').count(), SESSION_KEY_SEGMENTS);
        assert!(parse_session_key(&key).is_some());
    }

    #[test]
    fn is_safe_segment_rules() {
        assert!(is_safe_segment("peer-1_x.2"));
        assert!(!is_safe_segment(""));
        assert!(!is_safe_segment("../evil"));
        assert!(!is_safe_segment("a/b"));
        assert!(!is_safe_segment("a\\b"));
        assert!(!is_safe_segment("has space"));
    }
}
