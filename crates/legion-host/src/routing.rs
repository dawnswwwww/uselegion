use legion_core::config::{Binding, BindingMatch, Config};
use legion_core::util::iso_now;
use legion_plugin_sdk::channel::{InboundMessage, PeerKind};
use legion_plugin_sdk::session_key::{SessionKeyParts, build_session_key, parse_session_key};

/// Resolves inbound messages to an `agent_id` using the configured bindings.
///
/// Bindings are evaluated in config order; the first match wins. A binding with
/// multiple match fields uses AND semantics, and any unspecified match field is
/// treated as a wildcard. If no binding matches, the router falls back to
/// `"main"`.
#[derive(Debug, Clone, Default)]
pub struct Router {
    bindings: Vec<Binding>,
}

impl Router {
    /// Create a router from the bindings section of a config.
    pub fn from_config(config: &Config) -> Self {
        Self {
            bindings: config.bindings.clone(),
        }
    }

    /// Create a router with an explicit list of bindings (useful in tests).
    pub fn new(bindings: Vec<Binding>) -> Self {
        Self { bindings }
    }

    /// Resolve the agent that should handle `msg`.
    pub fn resolve_agent(&self, msg: &InboundMessage) -> String {
        for binding in &self.bindings {
            if binding_matches(&binding.match_, msg) {
                return binding.agent_id.clone();
            }
        }
        "main".to_string()
    }
}

fn binding_matches(match_: &BindingMatch, msg: &InboundMessage) -> bool {
    if let Some(channel) = &match_.channel {
        if channel != &msg.channel {
            return false;
        }
    }

    if let Some(account_id) = &match_.account_id {
        if account_id != "*" && account_id != &msg.account_id {
            return false;
        }
    }

    if let Some(peer) = &match_.peer {
        let kind_matches = match msg.peer.kind {
            PeerKind::Direct => peer.kind == "direct",
            PeerKind::Group => peer.kind == "group",
            PeerKind::Thread => peer.kind == "thread",
        };
        if !kind_matches || peer.id != msg.peer.id {
            return false;
        }
    }

    if let Some(guild_id) = &match_.guild_id {
        if msg.guild_id.as_ref() != Some(guild_id) {
            return false;
        }
    }

    if let Some(team_id) = &match_.team_id {
        if msg.team_id.as_ref() != Some(team_id) {
            return false;
        }
    }

    true
}

/// Build the synthetic inbound message used to route a session key through
/// the configured bindings. Shared by the WS `agent` RPC (`crate::turn`) and
/// [`resolve_session_key`].
pub(crate) fn build_router_message(parts: &SessionKeyParts, content: &str) -> InboundMessage {
    InboundMessage {
        channel: parts.channel.clone(),
        account_id: parts.account_id.clone(),
        peer: legion_plugin_sdk::channel::Peer {
            kind: parts.peer_kind.clone(),
            id: parts.peer_id.clone(),
            name: None,
            thread_id: None,
        },
        sender: legion_plugin_sdk::channel::Sender {
            id: parts.peer_id.clone(),
            display_name: None,
            username: None,
        },
        message_id: "rpc".into(),
        text: Some(content.into()),
        media: vec![],
        reply_to: None,
        timestamp: iso_now(),
        is_mentioned: false,
        ambient: false,
        guild_id: None,
        team_id: None,
    }
}

/// Resolve the raw session key into the final session key used by the runtime.
///
/// The agent id is selected via the configured bindings and the session key is
/// rebuilt so that workspace/auth profiles align with the binding.
pub fn resolve_session_key(session_key: &str, router: &Router) -> Option<String> {
    let parts = parse_session_key(session_key)?;
    let router_msg = build_router_message(&parts, "");
    let agent_id = router.resolve_agent(&router_msg);
    Some(build_session_key(&agent_id, &parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_core::config::{Binding, BindingMatch, PeerMatch};
    use legion_plugin_sdk::channel::{Peer, PeerKind, Sender};

    fn msg(channel: &str, account_id: &str, peer_kind: PeerKind, peer_id: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.into(),
            account_id: account_id.into(),
            peer: Peer {
                kind: peer_kind,
                id: peer_id.into(),
                name: None,
                thread_id: None,
            },
            sender: Sender {
                id: "sender".into(),
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

    #[test]
    fn falls_back_to_main_when_no_bindings() {
        let router = Router::default();
        assert_eq!(
            router.resolve_agent(&msg("telegram", "default", PeerKind::Direct, "u1")),
            "main"
        );
    }

    #[test]
    fn matches_channel_and_account() {
        let router = Router::new(vec![Binding {
            agent_id: "work".into(),
            match_: BindingMatch {
                channel: Some("slack".into()),
                account_id: Some("work".into()),
                ..Default::default()
            },
        }]);

        assert_eq!(
            router.resolve_agent(&msg("slack", "work", PeerKind::Direct, "u1")),
            "work"
        );
        assert_eq!(
            router.resolve_agent(&msg("telegram", "work", PeerKind::Direct, "u1")),
            "main"
        );
        assert_eq!(
            router.resolve_agent(&msg("slack", "personal", PeerKind::Direct, "u1")),
            "main"
        );
    }

    #[test]
    fn wildcard_account_matches_any_account() {
        let router = Router::new(vec![Binding {
            agent_id: "support".into(),
            match_: BindingMatch {
                channel: Some("telegram".into()),
                account_id: Some("*".into()),
                ..Default::default()
            },
        }]);

        assert_eq!(
            router.resolve_agent(&msg("telegram", "default", PeerKind::Direct, "u1")),
            "support"
        );
        assert_eq!(
            router.resolve_agent(&msg("telegram", "prod", PeerKind::Direct, "u1")),
            "support"
        );
        assert_eq!(
            router.resolve_agent(&msg("webchat", "default", PeerKind::Direct, "u1")),
            "main"
        );
    }

    #[test]
    fn peer_specific_override() {
        let router = Router::new(vec![
            Binding {
                agent_id: "main".into(),
                match_: BindingMatch {
                    channel: Some("telegram".into()),
                    account_id: Some("default".into()),
                    ..Default::default()
                },
            },
            Binding {
                agent_id: "escalation".into(),
                match_: BindingMatch {
                    channel: Some("telegram".into()),
                    account_id: Some("default".into()),
                    peer: Some(PeerMatch {
                        kind: "group".into(),
                        id: "g1".into(),
                    }),
                    ..Default::default()
                },
            },
        ]);

        assert_eq!(
            router.resolve_agent(&msg("telegram", "default", PeerKind::Direct, "u1")),
            "main"
        );
        assert_eq!(
            router.resolve_agent(&msg("telegram", "default", PeerKind::Group, "g1")),
            "main"
        );
    }

    #[test]
    fn peer_override_when_listed_first() {
        let router = Router::new(vec![
            Binding {
                agent_id: "escalation".into(),
                match_: BindingMatch {
                    channel: Some("telegram".into()),
                    account_id: Some("default".into()),
                    peer: Some(PeerMatch {
                        kind: "group".into(),
                        id: "g1".into(),
                    }),
                    ..Default::default()
                },
            },
            Binding {
                agent_id: "main".into(),
                match_: BindingMatch {
                    channel: Some("telegram".into()),
                    account_id: Some("default".into()),
                    ..Default::default()
                },
            },
        ]);

        assert_eq!(
            router.resolve_agent(&msg("telegram", "default", PeerKind::Group, "g1")),
            "escalation"
        );
        assert_eq!(
            router.resolve_agent(&msg("telegram", "default", PeerKind::Direct, "u1")),
            "main"
        );
    }

    #[test]
    fn multiple_match_fields_use_and_semantics() {
        let router = Router::new(vec![Binding {
            agent_id: "work".into(),
            match_: BindingMatch {
                channel: Some("slack".into()),
                account_id: Some("work".into()),
                peer: Some(PeerMatch {
                    kind: "group".into(),
                    id: "C123".into(),
                }),
                ..Default::default()
            },
        }]);

        assert_eq!(
            router.resolve_agent(&msg("slack", "work", PeerKind::Group, "C123")),
            "work"
        );
        assert_eq!(
            router.resolve_agent(&msg("slack", "work", PeerKind::Group, "C999")),
            "main"
        );
        assert_eq!(
            router.resolve_agent(&msg("slack", "personal", PeerKind::Group, "C123")),
            "main"
        );
    }

    #[test]
    fn guild_id_and_team_id_are_matched() {
        let mut message = msg("discord", "default", PeerKind::Direct, "u1");
        message.guild_id = Some("g123".into());
        message.team_id = Some("t456".into());

        let router = Router::new(vec![Binding {
            agent_id: "gaming".into(),
            match_: BindingMatch {
                channel: Some("discord".into()),
                guild_id: Some("g123".into()),
                team_id: Some("t456".into()),
                ..Default::default()
            },
        }]);

        assert_eq!(router.resolve_agent(&message), "gaming");

        message.guild_id = Some("other".into());
        assert_eq!(router.resolve_agent(&message), "main");
    }

    // ---- resolve_session_key ----

    fn bound_router() -> Router {
        Router::new(vec![Binding {
            agent_id: "work".to_string(),
            match_: BindingMatch {
                channel: Some("slack".to_string()),
                account_id: Some("acme".to_string()),
                ..BindingMatch::default()
            },
        }])
    }

    #[test]
    fn resolve_session_key_rebuilds_with_bound_agent() {
        let router = bound_router();
        let resolved = resolve_session_key("agent:main:dm:slack:acme:direct:u1", &router).unwrap();
        assert_eq!(resolved, "agent:work:dm:slack:acme:direct:u1");
    }

    #[test]
    fn resolve_session_key_unmatched_keeps_default_agent() {
        let router = bound_router();
        // No binding matches this channel/account: falls back to "main".
        let resolved =
            resolve_session_key("agent:main:dm:telegram:default:direct:u1", &router).unwrap();
        assert_eq!(resolved, "agent:main:dm:telegram:default:direct:u1");

        // Empty router: key passes through unchanged.
        let resolved = resolve_session_key(
            "agent:main:dm:telegram:default:direct:u1",
            &Router::default(),
        )
        .unwrap();
        assert_eq!(resolved, "agent:main:dm:telegram:default:direct:u1");
    }

    #[test]
    fn resolve_session_key_rejects_invalid_key() {
        assert!(resolve_session_key("bogus", &Router::default()).is_none());
    }
}
