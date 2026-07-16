use legion_channel::TelegramProvider;
use legion_plugin_sdk::channel::{ChannelProvider, OutboundMessage, PeerKind};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path, query_param},
};

#[tokio::test]
async fn should_poll_get_updates_and_send_message() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{}/getUpdates", "test-token")))
        .and(query_param("limit", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [
                {
                    "update_id": 1,
                    "message": {
                        "message_id": 10,
                        "from": { "id": 42, "first_name": "Test", "username": "tester" },
                        "chat": { "id": 42, "type": "private" },
                        "date": 1620000000,
                        "text": "hello from telegram"
                    }
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/bot{}/sendMessage", "test-token")))
        .and(body_json(json!({
            "chat_id": 42,
            "text": "hello back"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {
                "message_id": 11,
                "chat": { "id": 42, "type": "private" },
                "date": 1620000001,
                "text": "hello back"
            }
        })))
        .mount(&mock_server)
        .await;

    let provider = TelegramProvider::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    let config = json!({
        "token": "test-token",
        "base_url": mock_server.uri(),
        "account_id": "test-account"
    });

    provider.start(config, tx).await.unwrap();

    // Wait for the long-polling task to fetch and convert the update.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for inbound message")
        .expect("no message received");

    assert_eq!(msg.channel, "telegram");
    assert_eq!(msg.account_id, "test-account");
    assert_eq!(msg.peer.kind, PeerKind::Direct);
    assert_eq!(msg.peer.id, "42");
    assert_eq!(msg.sender.id, "42");
    assert_eq!(msg.sender.username, Some("tester".into()));
    assert_eq!(msg.text, Some("hello from telegram".into()));

    // Send an outbound reply through the provider.
    let outbound = OutboundMessage {
        channel: "telegram".into(),
        account_id: "test-account".into(),
        peer: legion_plugin_sdk::channel::Peer {
            kind: PeerKind::Direct,
            id: "42".into(),
            name: None,
            thread_id: None,
        },
        text: Some("hello back".into()),
        media: vec![],
        reply_to: None,
    };
    provider.send(outbound).await.unwrap();

    provider.stop().await.unwrap();
}

#[tokio::test]
async fn should_detect_group_vs_dm_peer_kind() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{}/getUpdates", "group-token")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [
                {
                    "update_id": 2,
                    "message": {
                        "message_id": 20,
                        "from": { "id": 100, "first_name": "User" },
                        "chat": { "id": -100123456, "type": "supergroup", "title": "Legion Group" },
                        "date": 1620000002,
                        "text": "group message"
                    }
                }
            ]
        })))
        .mount(&mock_server)
        .await;

    let provider = TelegramProvider::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    provider
        .start(
            json!({
                "token": "group-token",
                "base_url": mock_server.uri(),
                "account_id": "group-account"
            }),
            tx,
        )
        .await
        .unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for inbound message")
        .expect("no message received");

    assert_eq!(msg.peer.kind, PeerKind::Group);
    assert_eq!(msg.peer.id, "-100123456");
    assert_eq!(msg.peer.name, Some("Legion Group".into()));
    assert_eq!(msg.sender.id, "100");

    provider.stop().await.unwrap();
}
