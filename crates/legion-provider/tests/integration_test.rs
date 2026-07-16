use futures::StreamExt;
use legion_provider::{
    anthropic::AnthropicProvider, auth::AuthProfile, openai::OpenAiProvider, provider::Provider,
    types::ChatMessage,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn openai_sse_body(chunks: &[&str]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn openai_provider_streams_chat_completion() {
    let server = MockServer::start().await;

    let chunk1 = r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
    let chunk2 = r#"{"id":"c2","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":" there"},"finish_reason":"stop"}]}"#;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(openai_sse_body(&[chunk1, chunk2]))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(
        "openai",
        Some(server.uri()),
        AuthProfile::api_key("sk-test"),
    )
    .unwrap();

    let req =
        legion_provider::types::ChatRequest::new("gpt-4", vec![ChatMessage::user("Say hello")]);

    let mut stream = provider.chat(req).await.unwrap();
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        text.push_str(&chunk.unwrap().delta);
    }

    assert_eq!(text, "Hello there");
}

#[tokio::test]
async fn openai_provider_returns_embeddings() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                { "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] }
            ]
        })))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(
        "openai",
        Some(server.uri()),
        AuthProfile::api_key("sk-test"),
    )
    .unwrap();
    let req = legion_provider::types::EmbedRequest {
        model: "text-embedding-3-small".to_string(),
        input: vec!["hello".to_string()],
        extra: Default::default(),
    };

    let embeddings = provider.embed(req).await.unwrap();
    assert_eq!(embeddings.len(), 1);
    assert_eq!(embeddings[0].embedding, vec![0.1, 0.2, 0.3]);
}

fn anthropic_sse_body(events: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (event, data) in events {
        out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    }
    out
}

#[tokio::test]
async fn anthropic_provider_streams_chat_completion() {
    let server = MockServer::start().await;

    let events = vec![
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];

    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", "sk-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(anthropic_sse_body(&events))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(
        "anthropic",
        Some(server.uri()),
        AuthProfile::api_key("sk-test"),
    )
    .unwrap();

    let req = legion_provider::types::ChatRequest::new(
        "claude-sonnet-4-6",
        vec![ChatMessage::user("Say hi")],
    );

    let mut stream = provider.chat(req).await.unwrap();
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        text.push_str(&chunk.unwrap().delta);
    }

    assert_eq!(text, "Hi!");
}

#[tokio::test]
async fn openai_provider_propagates_http_errors() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider =
        OpenAiProvider::new("openai", Some(server.uri()), AuthProfile::api_key("bad")).unwrap();
    let req = legion_provider::types::ChatRequest::new("gpt-4", vec![ChatMessage::user("hi")]);

    assert!(provider.chat(req).await.is_err());
}

/// Live test against the MiniMax OpenAI-compatible endpoint.
/// Run with `cargo test -p legion-provider --test integration_test -- --ignored`
/// after setting `MINIMAX_API_KEY`.
#[tokio::test]
#[ignore = "requires MINIMAX_API_KEY env var"]
async fn minimax_openai_live_chat() {
    let key = std::env::var("MINIMAX_API_KEY").expect("MINIMAX_API_KEY must be set");
    let provider = OpenAiProvider::new(
        "minimax-openai",
        Some("https://api.minimaxi.com/v1".to_string()),
        AuthProfile::api_key(key),
    )
    .unwrap();

    let req = legion_provider::types::ChatRequest::new(
        "MiniMax-M3",
        vec![ChatMessage::user("Say a one-word greeting")],
    );

    let mut stream = provider.chat(req).await.unwrap();
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        text.push_str(&chunk.unwrap().delta);
    }

    assert!(
        !text.trim().is_empty(),
        "expected non-empty response from MiniMax"
    );
    println!("MiniMax response: {text}");
}

/// Live test that sends a tool result back to MiniMax to verify the message
/// format is accepted. Uses a fabricated assistant tool call, so a 400 about
/// the tool_call_id would indicate a format mismatch rather than a model issue.
#[tokio::test]
#[ignore = "requires MINIMAX_API_KEY env var"]
async fn minimax_openai_live_tool_result_format() {
    use legion_provider::types::{ChatRole, FunctionCall, ToolCall};

    let key = std::env::var("MINIMAX_API_KEY").expect("MINIMAX_API_KEY must be set");
    let provider = OpenAiProvider::new(
        "minimax-openai",
        Some("https://api.minimaxi.com/v1".to_string()),
        AuthProfile::api_key(key),
    )
    .unwrap();

    let mut assistant = ChatMessage::assistant("<think>using exec</think>");
    assistant.tool_calls = Some(vec![ToolCall {
        id: "call_abc123".to_string(),
        kind: "function".to_string(),
        function: FunctionCall {
            name: "exec".to_string(),
            arguments: r#"{"cmd":"pwd"}"#.to_string(),
        },
    }]);

    let tool = ChatMessage {
        role: ChatRole::Tool,
        content: "/Users/ringconn/workspace/projects/legion".to_string(),
        name: None,
        tool_calls: None,
        tool_call_id: Some("call_abc123".to_string()),
        cache_breakpoint: false,
    };

    let req = legion_provider::types::ChatRequest::new(
        "MiniMax-M3",
        vec![ChatMessage::user("what directory am i in"), assistant, tool],
    );

    let mut stream = provider.chat(req).await.unwrap();
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        text.push_str(&chunk.unwrap().delta);
    }

    println!("MiniMax tool-result response: {text}");
    assert!(
        !text.trim().is_empty(),
        "expected non-empty response after tool result"
    );
}
