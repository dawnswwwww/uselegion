//! Channel access control engine (channels gap Phase A).
//!
//! The config schema long declared `dmPolicy` / `allowlist` /
//! `requireMention`, but nothing enforced them — any DM could trigger the
//! agent. This module makes access control real: every inbound message is
//! evaluated against the channel's `access` policy before it reaches the
//! runtime, and a [`BotLoopGuard`] suppresses agent↔agent reply loops.
//!
//! Defaults are minimal-privilege on purpose: with no `access` block
//! configured, DMs are denied (empty allowlist) and group messages require a
//! mention. Set `channels.<id>.access.dmPolicy: "open"` to opt out.

use legion_core::config::Config;
use legion_plugin_sdk::channel::{InboundMessage, PeerKind};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Access policy for one channel (`channels.<id>.access`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessPolicy {
    /// How direct messages are gated (default `allowlist`).
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Sender ids allowed to DM the agent (used by `allowlist` and `pairing`).
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Group/thread policy.
    #[serde(default)]
    pub groups: GroupPolicy,
}

/// Direct-message policy.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// Anyone may DM the agent. Opt-in only; logs a warning at evaluation.
    Open,
    /// Only senders in `access.allowlist` may DM the agent (default).
    #[default]
    Allowlist,
    /// Only paired/allowlisted senders may DM the agent. Pairing state lives
    /// in the gateway's `PairingStore`; at this layer an allowlisted sender
    /// passes and everyone else is reported as not paired.
    Pairing,
}

/// Group/thread access policy.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GroupPolicy {
    /// Whether the agent only answers when mentioned (default true).
    #[serde(default = "default_require_mention")]
    pub require_mention: bool,
    /// Group peer ids the agent may answer in (empty = all groups).
    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl Default for GroupPolicy {
    fn default() -> Self {
        Self {
            require_mention: default_require_mention(),
            allowlist: Vec::new(),
        }
    }
}

fn default_require_mention() -> bool {
    true
}

/// Outcome of evaluating an inbound message against an access policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    Allow,
    Deny(DenyReason),
    /// Group message without a mention while `requireMention` is on.
    RequireMention,
}

/// Why an inbound message was denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    NotInAllowlist,
    NotPaired,
    BotLoop,
}

/// Parse the access policy for a channel from `channels.<id>.access`,
/// falling back to the secure default (allowlist DMs + requireMention).
pub fn policy_for(config: &Config, channel: &str) -> AccessPolicy {
    config
        .channels
        .get(channel)
        .and_then(|v| v.get("access").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Evaluate an inbound message against the access policy.
pub fn evaluate(msg: &InboundMessage, policy: &AccessPolicy) -> AccessDecision {
    match msg.peer.kind {
        PeerKind::Direct => match policy.dm_policy {
            DmPolicy::Open => AccessDecision::Allow,
            DmPolicy::Allowlist => {
                if policy.allowlist.iter().any(|id| id == &msg.sender.id) {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny(DenyReason::NotInAllowlist)
                }
            }
            DmPolicy::Pairing => {
                if policy.allowlist.iter().any(|id| id == &msg.sender.id) {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny(DenyReason::NotPaired)
                }
            }
        },
        PeerKind::Group | PeerKind::Thread => {
            if !policy.groups.allowlist.is_empty()
                && !policy.groups.allowlist.iter().any(|id| id == &msg.peer.id)
            {
                return AccessDecision::Deny(DenyReason::NotInAllowlist);
            }
            if policy.groups.require_mention && !msg.is_mentioned {
                return AccessDecision::RequireMention;
            }
            AccessDecision::Allow
        }
    }
}

/// Guards against agent↔agent reply loops: once we have replied to a
/// (channel, peer) `max_replies` times within `window`, further inbound from
/// that peer is treated as a suspected loop and denied. The guard records
/// outbound replies, not inbound traffic, so a talkative human alone never
/// trips it — only our own reply cadence does.
pub struct BotLoopGuard {
    window: Duration,
    max_replies: usize,
    recent: Mutex<HashMap<(String, String), VecDeque<Instant>>>,
}

impl BotLoopGuard {
    pub fn new(window: Duration, max_replies: usize) -> Self {
        Self {
            window,
            max_replies,
            recent: Mutex::new(HashMap::new()),
        }
    }

    /// `true` allows the inbound message; `false` marks a suspected loop.
    pub fn check_inbound(&self, channel: &str, peer_id: &str) -> bool {
        let recent = match self.recent.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let key = (channel.to_string(), peer_id.to_string());
        let Some(queue) = recent.get(&key) else {
            return true;
        };
        let now = Instant::now();
        let in_window = queue
            .iter()
            .filter(|t| now.duration_since(**t) <= self.window)
            .count();
        in_window < self.max_replies
    }

    /// Record one outbound reply to (channel, peer).
    pub fn record_outbound(&self, channel: &str, peer_id: &str) {
        let mut recent = match self.recent.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let key = (channel.to_string(), peer_id.to_string());
        let queue = recent.entry(key).or_default();
        let now = Instant::now();
        queue.push_back(now);
        while queue
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            queue.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_plugin_sdk::channel::{Peer, Sender};

    fn dm(sender_id: &str) -> InboundMessage {
        InboundMessage {
            channel: "telegram".into(),
            account_id: "default".into(),
            peer: Peer {
                kind: PeerKind::Direct,
                id: sender_id.into(),
                name: None,
                thread_id: None,
            },
            sender: Sender {
                id: sender_id.into(),
                display_name: None,
                username: None,
            },
            message_id: "m1".into(),
            text: Some("hi".into()),
            media: vec![],
            reply_to: None,
            timestamp: "t".into(),
            is_mentioned: false,
            ambient: false,
            guild_id: None,
            team_id: None,
        }
    }

    fn group_msg(group_id: &str, mentioned: bool) -> InboundMessage {
        let mut msg = dm("u1");
        msg.peer = Peer {
            kind: PeerKind::Group,
            id: group_id.into(),
            name: None,
            thread_id: None,
        };
        msg.is_mentioned = mentioned;
        msg
    }

    #[test]
    fn dm_open_allows_everyone() {
        let policy = AccessPolicy {
            dm_policy: DmPolicy::Open,
            ..Default::default()
        };
        assert_eq!(evaluate(&dm("stranger"), &policy), AccessDecision::Allow);
    }

    #[test]
    fn dm_allowlist_denies_strangers_by_default() {
        let policy = AccessPolicy::default();
        assert_eq!(
            evaluate(&dm("stranger"), &policy),
            AccessDecision::Deny(DenyReason::NotInAllowlist)
        );
    }

    #[test]
    fn dm_allowlist_allows_listed_sender() {
        let policy = AccessPolicy {
            allowlist: vec!["tg:123".into()],
            ..Default::default()
        };
        assert_eq!(evaluate(&dm("tg:123"), &policy), AccessDecision::Allow);
    }

    #[test]
    fn dm_pairing_reports_not_paired() {
        let policy = AccessPolicy {
            dm_policy: DmPolicy::Pairing,
            ..Default::default()
        };
        assert_eq!(
            evaluate(&dm("stranger"), &policy),
            AccessDecision::Deny(DenyReason::NotPaired)
        );
    }

    #[test]
    fn group_requires_mention_by_default() {
        let policy = AccessPolicy::default();
        assert_eq!(
            evaluate(&group_msg("g1", false), &policy),
            AccessDecision::RequireMention
        );
        assert_eq!(
            evaluate(&group_msg("g1", true), &policy),
            AccessDecision::Allow
        );
    }

    #[test]
    fn group_allowlist_restricts_groups() {
        let policy = AccessPolicy {
            groups: GroupPolicy {
                require_mention: false,
                allowlist: vec!["g1".into()],
            },
            ..Default::default()
        };
        assert_eq!(
            evaluate(&group_msg("g2", false), &policy),
            AccessDecision::Deny(DenyReason::NotInAllowlist)
        );
        assert_eq!(
            evaluate(&group_msg("g1", false), &policy),
            AccessDecision::Allow
        );
    }

    #[test]
    fn policy_for_defaults_when_unconfigured() {
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let policy = policy_for(&config, "telegram");
        assert_eq!(policy.dm_policy, DmPolicy::Allowlist);
        assert!(policy.groups.require_mention);
    }

    #[test]
    fn policy_for_parses_access_block() {
        let config = Config::from_json(
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "channels": {
                    "telegram": {
                        "access": {
                            "dmPolicy": "open",
                            "allowlist": ["tg:1"],
                            "groups": { "requireMention": false, "allowlist": ["g9"] }
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let policy = policy_for(&config, "telegram");
        assert_eq!(policy.dm_policy, DmPolicy::Open);
        assert_eq!(policy.allowlist, vec!["tg:1"]);
        assert!(!policy.groups.require_mention);
        assert_eq!(policy.groups.allowlist, vec!["g9"]);
    }

    #[test]
    fn bot_loop_guard_trips_after_max_replies() {
        let guard = BotLoopGuard::new(Duration::from_secs(3600), 3);
        assert!(guard.check_inbound("telegram", "p1"));
        guard.record_outbound("telegram", "p1");
        guard.record_outbound("telegram", "p1");
        assert!(guard.check_inbound("telegram", "p1"));
        guard.record_outbound("telegram", "p1");
        assert!(!guard.check_inbound("telegram", "p1"));
        // Other peers are unaffected.
        assert!(guard.check_inbound("telegram", "p2"));
    }
}
