use legion_channel::webchat_inbound;
use legion_core::config::Config;
use legion_host::routing::Router;
use legion_plugin_sdk::channel::{InboundMessage, Peer, PeerKind, Sender};

fn routing_config() -> Config {
    Config::from_json(
        r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": { "workspace": "~/.legion/workspace" },
                "list": [
                    { "id": "work", "workspace": "~/.legion/workspace-work" }
                ]
            },
            "bindings": [
                { "agentId": "work", "match": { "channel": "slack", "accountId": "work" } },
                { "agentId": "escalation", "match": { "channel": "telegram", "accountId": "default", "peer": { "kind": "group", "id": "alerts" } } },
                { "agentId": "main", "match": { "channel": "telegram", "accountId": "*" } }
            ]
        }"#,
    )
    .unwrap()
}

fn telegram_msg(account_id: &str, peer_kind: PeerKind, peer_id: &str) -> InboundMessage {
    InboundMessage {
        channel: "telegram".into(),
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
fn routes_webchat_to_main_by_default() {
    let router = Router::from_config(&routing_config());
    let msg = webchat_inbound("user-1", "hello");

    assert_eq!(router.resolve_agent(&msg), "main");
}

#[test]
fn routes_slack_work_account_to_work_agent() {
    let router = Router::from_config(&routing_config());
    let mut msg = webchat_inbound("user-1", "hello");
    msg.channel = "slack".into();
    msg.account_id = "work".into();

    assert_eq!(router.resolve_agent(&msg), "work");
}

#[test]
fn routes_telegram_group_alerts_to_escalation() {
    let router = Router::from_config(&routing_config());
    let msg = telegram_msg("default", PeerKind::Group, "alerts");

    assert_eq!(router.resolve_agent(&msg), "escalation");
}

#[test]
fn routes_telegram_dm_to_main_via_wildcard() {
    let router = Router::from_config(&routing_config());
    let msg = telegram_msg("default", PeerKind::Direct, "u123");

    assert_eq!(router.resolve_agent(&msg), "main");
}

#[test]
fn routes_telegram_any_account_to_main() {
    let router = Router::from_config(&routing_config());
    let msg = telegram_msg("other-account", PeerKind::Direct, "u123");

    assert_eq!(router.resolve_agent(&msg), "main");
}
