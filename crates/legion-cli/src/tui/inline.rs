//! Inline / minimal viewport support.
//!
//! In inline mode the TUI does not take over the full alternate screen.
//! Instead, ratatui's `Viewport::Inline` renders a small live area at the
//! bottom of the normal terminal scrollback. Messages that have finished
//! (user turns and finalized assistant/tool output) are emitted as plain text
//! to the scrollback so the user can use their terminal's native scrolling.

use crate::tui::state::{
    AppState, ChatMessage, MessageRole, MessageState, ScreenMode, TOOL_ARGS_MAX_CHARS,
};
use crate::tui::tool_card::{parse_tool_card, truncate_chars};
use std::io;

/// Height of the inline live viewport in terminal rows.
pub(crate) const INLINE_HEIGHT: u16 = 6;

/// Render a tool card as plain scrollback text (inline mode has no styling).
fn tool_card_to_scrollback(content: &str) -> String {
    let (state, name, arguments, result) = parse_tool_card(content);
    let mut out = format!("\n▸ {name} · {state}\n");
    if let Some(args) = arguments {
        out.push_str(&format!(
            "│ args: {}\n",
            truncate_chars(&args, TOOL_ARGS_MAX_CHARS)
        ));
    }
    if let Some(res) = result {
        for line in res.lines().take(20) {
            out.push_str(&format!("│ {line}\n"));
        }
        if res.lines().count() > 20 {
            out.push_str("│ …\n");
        }
    }
    out
}

/// Render a finalized message as plain scrollback text.
fn message_to_scrollback(msg: &ChatMessage) -> String {
    match msg.role {
        MessageRole::User => format!("\n> {}\n", msg.content.trim()),
        MessageRole::Assistant => {
            if msg.state == MessageState::Streaming {
                String::new()
            } else {
                format!("\n{}\n", msg.content.trim())
            }
        }
        MessageRole::Tool => tool_card_to_scrollback(&msg.content),
        MessageRole::System => format!("\n[system] {}\n", msg.content.trim()),
        MessageRole::Question => format!("\n[question] {}\n", msg.content.trim()),
    }
}

/// Emit any finalized messages that have not yet been written to scrollback.
/// `send` writes bytes to the terminal via the writer thread.
pub(crate) fn emit_finalized_messages<F>(state: &mut AppState, mut send: F) -> io::Result<()>
where
    F: FnMut(Vec<u8>) -> io::Result<()>,
{
    if state.screen_mode != ScreenMode::Inline {
        return Ok(());
    }
    let next = state.messages.len();
    let start = state.last_emitted_scrollback_index;
    if start >= next {
        return Ok(());
    }
    let mut out = String::new();
    for msg in &state.messages[start..next] {
        out.push_str(&message_to_scrollback(msg));
    }
    if !out.is_empty() {
        send(out.into_bytes())?;
    }
    state.last_emitted_scrollback_index = next;
    Ok(())
}

/// Mark all current messages as already emitted, used when switching *into*
/// inline mode so we do not dump the entire history onto the current line.
pub(crate) fn reset_emitted_index(state: &mut AppState) {
    state.last_emitted_scrollback_index = state.messages.len();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_messages(mode: ScreenMode) -> (AppState, Vec<u8>) {
        let state = AppState {
            screen_mode: mode,
            ..Default::default()
        };
        let captured = Vec::new();
        (state, captured)
    }

    #[test]
    fn emits_nothing_in_fullscreen_mode() {
        let (mut state, mut captured) = state_with_messages(ScreenMode::Fullscreen);
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "hello"));
        emit_finalized_messages(&mut state, |bytes| {
            captured.extend_from_slice(&bytes);
            Ok(())
        })
        .unwrap();
        assert!(captured.is_empty());
    }

    #[test]
    fn emits_finalized_user_and_assistant_messages() {
        let (mut state, mut captured) = state_with_messages(ScreenMode::Inline);
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "hello"));
        state
            .messages
            .push(ChatMessage::new(MessageRole::Assistant, "hi there"));
        emit_finalized_messages(&mut state, |bytes| {
            captured.extend_from_slice(&bytes);
            Ok(())
        })
        .unwrap();
        let text = String::from_utf8(captured).unwrap();
        assert!(text.contains("> hello"));
        assert!(text.contains("hi there"));
        assert_eq!(state.last_emitted_scrollback_index, 2);
    }

    #[test]
    fn skips_streaming_assistant_message() {
        let (mut state, mut captured) = state_with_messages(ScreenMode::Inline);
        let mut assistant = ChatMessage::new(MessageRole::Assistant, "typing");
        assistant.state = MessageState::Streaming;
        state.messages.push(assistant);
        emit_finalized_messages(&mut state, |bytes| {
            captured.extend_from_slice(&bytes);
            Ok(())
        })
        .unwrap();
        assert!(captured.is_empty());
    }

    #[test]
    fn only_emits_new_messages_on_subsequent_calls() {
        let (mut state, mut captured) = state_with_messages(ScreenMode::Inline);
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "first"));
        emit_finalized_messages(&mut state, |bytes| {
            captured.extend_from_slice(&bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(state.last_emitted_scrollback_index, 1);

        captured.clear();
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "second"));
        emit_finalized_messages(&mut state, |bytes| {
            captured.extend_from_slice(&bytes);
            Ok(())
        })
        .unwrap();
        let text = String::from_utf8(captured).unwrap();
        assert!(!text.contains("first"));
        assert!(text.contains("second"));
    }

    #[test]
    fn tool_message_emits_formatted_card_not_json() {
        let (mut state, mut captured) = state_with_messages(ScreenMode::Inline);
        state.messages.push(ChatMessage::new(
            MessageRole::Tool,
            crate::tui::tool_card::tool_card_json(
                "done",
                "exec",
                Some("{\"cmd\":\"ls\"}"),
                Some("file1\nfile2"),
            ),
        ));
        emit_finalized_messages(&mut state, |bytes| {
            captured.extend_from_slice(&bytes);
            Ok(())
        })
        .unwrap();
        let text = String::from_utf8(captured).unwrap();
        assert!(
            text.contains("▸ exec · done"),
            "card header missing: {text}"
        );
        assert!(text.contains("file1"), "result body missing: {text}");
        assert!(!text.contains("\"state\""), "raw JSON leaked: {text}");
    }
}
