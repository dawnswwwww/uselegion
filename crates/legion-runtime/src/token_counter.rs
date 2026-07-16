use legion_provider::types::ChatMessage;

/// Estimate the number of tokens in a piece of text using the cl100k_base
/// tokenizer (used by GPT-4 / Claude-style models as a close approximation).
pub fn count_tokens(text: &str) -> usize {
    // tiktoken-rs may fail to load if the embedded BPE data is unavailable;
    // fall back to a rough character heuristic so the runtime never panics.
    // The encoder is cached process-wide (`cl100k_bpe`) — building it per call
    // costs hundreds of milliseconds and stalls the agent loop.
    match legion_provider::ops::cl100k_bpe() {
        Some(bpe) => bpe.encode_with_special_tokens(text).len(),
        None => text.chars().count() / 4 + text.split_whitespace().count() / 2,
    }
}

/// Estimate the token cost of a single chat message.
///
/// This includes the content, role, name/tool identifiers, and tool calls.
/// A small per-message overhead is added to account for API framing.
pub fn estimate_message_tokens(msg: &ChatMessage) -> usize {
    let mut text = String::with_capacity(msg.content.len() + 64);
    text.push_str(&format!("{:?}\n", msg.role));
    if let Some(name) = &msg.name {
        text.push_str(name);
        text.push('\n');
    }
    text.push_str(&msg.content);
    if let Some(tool_calls) = &msg.tool_calls {
        for tc in tool_calls {
            text.push('\n');
            text.push_str(&tc.function.name);
            text.push('\n');
            text.push_str(&tc.function.arguments);
        }
    }
    if let Some(id) = &msg.tool_call_id {
        text.push('\n');
        text.push_str(id);
    }

    // Per-message overhead observed in OpenAI/Claude token accounting.
    count_tokens(&text).saturating_add(4)
}

/// Estimate the total token count for a list of messages plus a system prompt.
pub fn estimate_total_tokens(messages: &[ChatMessage], system_prompt: &str) -> usize {
    let messages_tokens: usize = messages.iter().map(estimate_message_tokens).sum();
    messages_tokens + count_tokens(system_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_provider::types::{ChatMessage, ChatRole, FunctionCall, ToolCall};

    #[test]
    fn count_tokens_is_positive() {
        assert!(count_tokens("hello world") > 0);
    }

    #[test]
    fn message_token_estimate_counts_content() {
        let short = ChatMessage::user("hi");
        let long = ChatMessage::user("word ".repeat(100));
        assert!(estimate_message_tokens(&long) > estimate_message_tokens(&short));
    }

    #[test]
    fn tool_call_adds_tokens() {
        let plain = ChatMessage::assistant("ok");
        let with_tool = ChatMessage {
            role: ChatRole::Assistant,
            content: "".to_string(),
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "read".to_string(),
                    arguments: r#"{"path":"x"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
            cache_breakpoint: false,
        };
        assert!(estimate_message_tokens(&with_tool) > estimate_message_tokens(&plain));
    }

    #[test]
    fn total_tokens_includes_system_prompt() {
        let msg = ChatMessage::user("hi");
        let with_prompt = estimate_total_tokens(std::slice::from_ref(&msg), "you are helpful");
        let without_prompt = estimate_total_tokens(std::slice::from_ref(&msg), "");
        assert!(with_prompt > without_prompt);
    }
}
