//! TUI input editing helpers.

use crate::tui::state::{AppState, PASTE_CHAR_THRESHOLD, PASTE_LINE_THRESHOLD};
use unicode_width::UnicodeWidthChar;

/// Apply a new maximum scroll position while preserving the user's intent.
///
/// - If `force_scroll_bottom` is set, snap to the bottom and clear the flag.
/// - If the user was already at the bottom, keep following new content.
/// - Otherwise keep the current scroll position, clamped to the new max.
pub(crate) fn apply_scroll(state: &mut AppState, max_scroll: usize) {
    let was_at_bottom = state.scroll >= state.max_scroll;
    state.max_scroll = max_scroll;
    if state.force_scroll_bottom || was_at_bottom {
        state.scroll = max_scroll;
        state.force_scroll_bottom = false;
    } else {
        state.scroll = state.scroll.min(max_scroll);
    }
}

pub(crate) fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1)
}

pub(crate) fn input_visual_lines(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for c in text.chars() {
        let w = char_width(c);
        if current_width + w > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(c);
        current_width += w;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Insert pasted text, collapsing large pastes into a placeholder token.
pub(crate) fn handle_paste(state: &mut AppState, text: String) {
    if text.is_empty() {
        return;
    }

    let line_count = text.lines().count();
    if text.len() > PASTE_CHAR_THRESHOLD || line_count > PASTE_LINE_THRESHOLD {
        let id = state.next_paste_id;
        state.next_paste_id += 1;
        let placeholder = format!(
            "[...Pasted text #{}: {} lines, {} chars...]",
            id,
            line_count,
            text.len()
        );
        state.paste_store.insert(placeholder.clone(), text);
        state.composer.insert_str(&placeholder);
    } else {
        state.composer.insert_str(&text);
    }
    state.slash_selected = 0;
}

pub(crate) fn expand_paste_placeholders(
    input: &str,
    store: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = input.to_string();
    for (placeholder, content) in store {
        result = result.replace(placeholder, content);
    }
    result
}

/// Record `text` in the session input history and clear the input box,
/// resetting all transient input state.
pub(crate) fn commit_and_clear_input(state: &mut AppState, text: &str) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        state.input_history.push(trimmed.to_string());
    }
    state.history_index = None;
    state.draft_input = None;
    state.composer.clear();
    state.slash_selected = 0;
    state.force_scroll_bottom = true;
    state.paste_store.clear();
}

/// Recall previous (↑) or next (↓) user input from `input_history`.
/// The current draft is saved on first ↑ and restored by ↓ past the newest
/// history entry. The cursor is placed at the end of the recalled text.
pub(crate) fn navigate_input_history(state: &mut AppState, up: bool) {
    if state.input_history.is_empty() {
        return;
    }
    if up {
        if state.history_index.is_none() {
            state.draft_input = Some(state.composer.join());
            state.history_index = Some(state.input_history.len() - 1);
        } else if let Some(idx) = state.history_index {
            if idx > 0 {
                state.history_index = Some(idx - 1);
            }
        }
    } else if let Some(idx) = state.history_index {
        if idx + 1 < state.input_history.len() {
            state.history_index = Some(idx + 1);
        } else {
            if let Some(draft) = state.draft_input.take() {
                state.composer.set_text(&draft);
            } else {
                state.composer.clear();
            }
            state.history_index = None;
            state.slash_selected = 0;
            return;
        }
    }
    if let Some(idx) = state.history_index {
        state.composer.set_text(&state.input_history[idx]);
        state.slash_selected = 0;
    }
}

/// Accept the highlighted slash-command completion: replace the input with
/// `/<name> ` and put the cursor at the end, closing the menu.
pub(crate) fn complete_slash_command(
    state: &mut AppState,
    cmd: &crate::slash_commands::SlashCommand,
) {
    state.composer.set_text(&format!("/{} ", cmd.name));
    state.slash_selected = 0;
}
