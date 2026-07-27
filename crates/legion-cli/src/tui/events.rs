//! TUI event handling.

use crate::slash_commands::CommandResult;
use crate::tui::history_search::HistorySearch;
use crate::tui::input::{complete_slash_command, navigate_input_history};
use crate::tui::question::format_question_message;
use crate::tui::selection::{Selection, osc52_copy, position_to_cursor, selected_text};
use crate::tui::state::{
    AppState, ChatMessage, MessageRole, MessageState, OutboundControl, PendingQuestion,
};
use crate::tui::tool_card::tool_card_json;
use crate::tui::widgets::{MessageSegment, parse_message_segments};
use crossterm::event::{self, KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use legion_runtime::{AskUserOutput, TodoItem, TodoStatus};
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// Send a user-typed message now, or queue it behind the in-flight run.
/// Queued messages do not appear in the chat until they are actually sent,
/// so streaming deltas always append to the last assistant message.
fn send_user_message(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
    text: String,
) {
    if state.is_active() {
        state.queued_messages.push_back((text, true));
    } else {
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, text.clone()));
        state.pending_request = true;
        let _ = send_tx.send(OutboundControl::Message(text));
    }
}

/// Send an agent-directed payload (slash-command/skill bodies): never
/// rendered in the chat, but still serialized behind an in-flight run.
fn send_agent_message(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
    message: String,
) {
    if state.is_active() {
        state.queued_messages.push_back((message, false));
    } else {
        state.pending_request = true;
        let _ = send_tx.send(OutboundControl::Message(message));
    }
}

/// Pop the oldest queued message into the chat and send it. Called when a
/// run lifecycle ends (normally, with an error, or cancelled).
fn drain_queued_message(state: &mut AppState, send_tx: &mpsc::UnboundedSender<OutboundControl>) {
    if let Some((text, show_in_chat)) = state.queued_messages.pop_front() {
        if show_in_chat {
            state
                .messages
                .push(ChatMessage::new(MessageRole::User, text.clone()));
        }
        state.pending_request = true;
        let _ = send_tx.send(OutboundControl::Message(text));
    }
}

/// Mutate state after a failed `run_turn`: surface the error, reset
/// `pending_request`, and discard any queued messages (their run never
/// started, so no lifecycle event will ever drain them).
pub(crate) fn fail_pending_send(state: &mut AppState, err: &str) {
    let mut msg = ChatMessage::new(MessageRole::System, format!("failed to send: {err}"));
    msg.state = MessageState::Error;
    state.messages.push(msg);
    state.pending_request = false;
    let dropped = state.queued_messages.len();
    if dropped > 0 {
        state.queued_messages.clear();
        state.messages.push(ChatMessage::new(
            MessageRole::System,
            format!("{dropped} queued message(s) discarded after send failure"),
        ));
    }
}

pub(crate) fn handle_key_event(
    state: &mut AppState,
    key: event::KeyEvent,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    // Ctrl+C copies the current scrollback selection if one exists; otherwise
    // it acts as a quit shortcut (alongside Ctrl+Q).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(ref sel) = state.selection {
            if !sel.is_empty() {
                let text = selected_text(sel, &state.messages, &state.render_cache);
                print!("{}", osc52_copy(&text));
                state.notice = Some((
                    format!("copied {} chars", text.chars().count()),
                    std::time::Instant::now(),
                ));
                state.selection = None;
                return;
            }
        }
        state.quit = true;
        return;
    }
    if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.quit = true;
        return;
    }
    // While an `ask_user` prompt is pending, key input is modal.
    if state.pending_question.is_some() {
        handle_question_key(state, key, send_tx);
        return;
    }
    // While a tool-approval prompt is pending, key input is modal: y/n (or
    // Esc) answer the prompt, quit shortcuts still work, and everything else
    // is swallowed so a stray keystroke cannot be misread as an answer.
    if state.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                resolve_pending_approval(state, true, send_tx);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_pending_approval(state, false, send_tx);
            }
            _ => {}
        }
        return;
    }
    // While the history search popup is open, keys navigate/accept/cancel it.
    if state.history_search.is_some() {
        handle_history_search_key(state, key);
        return;
    }
    // Computed once per key: the completion menu state for `/` input. It is
    // derived from the input alone, so every handler below sees the same view.
    let sugg = state.slash_suggestions();
    match key.code {
        // Shortcuts require Ctrl so plain typing always reaches the input
        // box — the input is permanently focused, there is no mode in which
        // a bare 'q'/'t' can be interpreted as a command.
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            toggle_nearest_thinking(state);
        }
        KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.composer.undo();
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.composer.redo();
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.history_search = Some(HistorySearch::new());
        }
        // Alt+Enter inserts a newline; plain Enter sends. (Shift+Enter is
        // indistinguishable from Enter without the kitty keyboard protocol,
        // which we do not enable.)
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            state.composer.insert_newline();
        }
        KeyCode::Enter => {
            if let Some(cmd) = sugg.get(state.slash_selected).cloned() {
                // The completion menu is open: Enter accepts the selection.
                // No-arg commands execute right away; commands that take
                // arguments only complete (like Tab) and wait for input.
                if cmd.arg_hint.is_empty() {
                    let text = format!("/{}", cmd.name);
                    match crate::slash_commands::dispatch(state, &text) {
                        CommandResult::Handled => {}
                        CommandResult::SendToAgent { message } => {
                            send_agent_message(state, send_tx, message);
                        }
                        CommandResult::ScheduleLoop { .. } => {
                            state.messages.push(ChatMessage::new(
                                MessageRole::System,
                                "Loop scheduling is only supported in gateway mode.".to_string(),
                            ));
                        }
                        CommandResult::NotACommand => {}
                    }
                    crate::tui::input::commit_and_clear_input(state, &text);
                } else {
                    complete_slash_command(state, &cmd);
                }
            } else {
                let text = crate::tui::input::expand_paste_placeholders(
                    &state.composer.join(),
                    &state.paste_store,
                );
                let text = text.trim().to_string();
                if !text.is_empty() {
                    // Shell escape mode: `!command` runs locally through the
                    // user's shell and shows the output in the chat.
                    if let Some(shell_cmd) = text.strip_prefix('!').map(str::trim) {
                        if shell_cmd.is_empty() {
                            state.messages.push(ChatMessage::new(
                                MessageRole::System,
                                "shell command is empty".to_string(),
                            ));
                        } else {
                            state
                                .messages
                                .push(ChatMessage::new(MessageRole::User, text.clone()));
                            let _ =
                                send_tx.send(OutboundControl::ShellCommand(shell_cmd.to_string()));
                        }
                        crate::tui::input::commit_and_clear_input(state, &text);
                    } else if text.starts_with('/') {
                        // Slash commands: builtins run locally; skill commands
                        // (/skills:<name>) inject the body and forward to the
                        // agent. Path-like text (`/tmp/x`) falls through.
                        match crate::slash_commands::dispatch(state, &text) {
                            CommandResult::Handled => {
                                crate::tui::input::commit_and_clear_input(state, &text);
                            }
                            CommandResult::SendToAgent { message } => {
                                crate::tui::input::commit_and_clear_input(state, &text);
                                send_agent_message(state, send_tx, message);
                            }
                            CommandResult::ScheduleLoop { .. } => {
                                state.messages.push(ChatMessage::new(
                                    MessageRole::System,
                                    "Loop scheduling is only supported in gateway mode."
                                        .to_string(),
                                ));
                                crate::tui::input::commit_and_clear_input(state, &text);
                            }
                            CommandResult::NotACommand => {
                                // Fall through: treat as a normal message.
                                crate::tui::input::commit_and_clear_input(state, &text);
                                send_user_message(state, send_tx, text);
                            }
                        }
                    } else {
                        // Queued behind an in-flight run when necessary. No
                        // empty assistant placeholder is added here either
                        // way; the assistant row is created lazily by
                        // handle_ws_event when the first delta arrives.
                        crate::tui::input::commit_and_clear_input(state, &text);
                        send_user_message(state, send_tx, text);
                    }
                }
            }
        }
        KeyCode::Tab => {
            if let Some(cmd) = sugg.get(state.slash_selected).cloned() {
                complete_slash_command(state, &cmd);
            }
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if sugg.is_empty() {
                state.composer.move_cursor_up();
            }
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if sugg.is_empty() {
                state.composer.move_cursor_down();
            }
        }
        KeyCode::Up => {
            if sugg.is_empty() {
                navigate_input_history(state, true);
            } else {
                // While the completion menu is open, ↑/↓ navigate it.
                state.slash_selected = if state.slash_selected == 0 {
                    sugg.len() - 1
                } else {
                    state.slash_selected - 1
                };
            }
        }
        KeyCode::Down => {
            if sugg.is_empty() {
                navigate_input_history(state, false);
            } else {
                state.slash_selected = (state.slash_selected + 1) % sugg.len();
            }
        }
        KeyCode::PageUp => {
            let delta = state.page_scroll_delta();
            state.scroll = state.scroll.saturating_sub(delta);
        }
        KeyCode::PageDown => {
            let delta = state.page_scroll_delta();
            state.scroll = state.scroll.saturating_add(delta);
        }
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = 0;
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = state.max_scroll;
        }
        KeyCode::Home if key.modifiers.contains(KeyModifiers::ALT) => {
            state.composer.move_cursor_top();
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::ALT) => {
            state.composer.move_cursor_bottom();
        }
        // Esc cancels the in-flight run. When idle it does nothing, matching
        // its previous behavior (tui-textarea ignores Esc).
        KeyCode::Esc => {
            if state.is_active() {
                let _ = send_tx.send(OutboundControl::Cancel);
                state.messages.push(ChatMessage::new(
                    MessageRole::System,
                    "cancelling run…".to_string(),
                ));
            }
        }
        // All other keys are handled by the rich textarea editor.
        _ => {
            state.composer.input(key);
        }
    }
}

/// Answer the pending tool-approval prompt: send the decision to the driver
/// (via the sender task) and leave a short note in the chat history.
pub(crate) fn resolve_pending_approval(
    state: &mut AppState,
    allow: bool,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    if let Some((prompt_id, tool)) = state.pending_approval.take() {
        let _ = send_tx.send(OutboundControl::ResolveApproval { prompt_id, allow });
        let decision = if allow { "approved" } else { "denied" };
        state.messages.push(ChatMessage::new(
            MessageRole::System,
            format!("tool '{tool}' {decision}"),
        ));
    }
}

/// Refresh the inline question message so selection changes are visible.
pub(crate) fn refresh_question_message(state: &mut AppState) {
    if let Some(pq) = state.pending_question.as_ref() {
        if let Some(msg) = state.messages.get_mut(pq.message_index) {
            msg.content = format_question_message(pq);
        }
    }
}

/// Handle keys while an `ask_user` prompt is pending.
///
/// Questions are presented as a horizontal tab bar with a final Submit tab.
/// Left/Right arrows switch tabs (wrapping), Up/Down navigate options within
/// the current question tab, Space toggles multi-select options, Enter selects
/// the focused option or submits on the Submit tab, and Esc cancels the prompt.
pub(crate) fn handle_question_key(
    state: &mut AppState,
    key: event::KeyEvent,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    let Some(pq) = state.pending_question.as_mut() else {
        return;
    };
    let tab_count = pq.tab_count();
    match key.code {
        KeyCode::Left => {
            pq.current = (pq.current + tab_count - 1) % tab_count;
            pq.focused = 0;
        }
        KeyCode::Right => {
            pq.current = (pq.current + 1) % tab_count;
            pq.focused = 0;
        }
        KeyCode::Up => {
            if !pq.is_submit_tab() && pq.focused > 0 {
                pq.focused -= 1;
            }
        }
        KeyCode::Down => {
            let option_count = pq.current_question().map(|q| q.options.len());
            if let Some(count) = option_count {
                if pq.focused + 1 < count {
                    pq.focused += 1;
                }
            }
        }
        KeyCode::Char(' ') => {
            if let Some(q) = pq.current_question() {
                if q.multi_select {
                    let question = q.question.clone();
                    let label = q.options[pq.focused].label.clone();
                    pq.toggle(&question, &label);
                }
            }
        }
        KeyCode::Enter => {
            if pq.is_submit_tab() {
                let pq = state
                    .pending_question
                    .take()
                    .expect("submit tab must exist");
                resolve_pending_question(state, send_tx, pq);
                return;
            }
            let (question, label, multi_select) = pq
                .current_question()
                .map(|q| {
                    (
                        q.question.clone(),
                        q.options[pq.focused].label.clone(),
                        q.multi_select,
                    )
                })
                .expect("question tab must have a question");
            if multi_select {
                pq.toggle(&question, &label);
            } else {
                pq.select_only(&question, &label);
            }
            refresh_question_message(state);
            return;
        }
        KeyCode::Esc => {
            cancel_pending_question(state, send_tx);
            return;
        }
        _ => {}
    }
    refresh_question_message(state);
}

/// Handle keys while the history-search popup is open.
pub(crate) fn handle_history_search_key(state: &mut AppState, key: event::KeyEvent) {
    let Some(ref mut hs) = state.history_search else {
        return;
    };
    let filtered = hs.filtered(&state.input_history);
    match key.code {
        KeyCode::Esc => {
            state.history_search = None;
        }
        KeyCode::Up => {
            hs.move_up(filtered.len());
        }
        KeyCode::Down => {
            hs.move_down(filtered.len());
        }
        KeyCode::Enter => {
            if let Some((_, text)) = filtered.get(hs.selected) {
                state.composer.set_text(text);
            }
            state.history_search = None;
        }
        KeyCode::Backspace => {
            hs.query.pop();
            hs.selected = 0;
        }
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            hs.query.push(c);
            hs.selected = 0;
        }
        _ => {}
    }
}

/// Route a bracketed-paste event. Pastes are modal-gated like keys: while an
/// approval or question prompt owns the keyboard the paste is dropped (the
/// input box is invisible, so stuffing text into it is never intended), and
/// while the history-search popup is open the paste extends the query.
pub(crate) fn route_paste(state: &mut AppState, text: String) {
    if let Some(ref mut hs) = state.history_search {
        hs.query.push_str(&text.replace(['\n', '\r'], " "));
        hs.selected = 0;
    } else if state.pending_approval.is_none() && state.pending_question.is_none() {
        crate::tui::input::handle_paste(state, text);
    }
}

/// Cancel the question prompt and answer with an empty selection so the run
/// does not hang forever.
pub(crate) fn cancel_pending_question(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    let Some(pq) = state.pending_question.take() else {
        return;
    };
    if let Some(msg) = state.messages.get_mut(pq.message_index) {
        msg.content = format!(
            "{}\n\n[cancelled]",
            msg.content.lines().next().unwrap_or("Question")
        );
    }
    let output = AskUserOutput {
        questions: pq.questions.clone(),
        answers: HashMap::new(),
        annotations: None,
    };
    let _ = send_tx.send(OutboundControl::ResolveQuestion {
        prompt_id: pq.prompt_id,
        output,
    });
    state.messages.push(ChatMessage::new(
        MessageRole::System,
        "question cancelled".to_string(),
    ));
}

/// Send the collected answers for a completed question prompt.
pub(crate) fn resolve_pending_question(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
    pq: PendingQuestion,
) {
    let prompt_id = pq.prompt_id.clone();
    let message_index = pq.message_index;
    let output = pq.into_output();
    if let Some(msg) = state.messages.get_mut(message_index) {
        let mut lines = Vec::new();
        for q in &output.questions {
            lines.push(format!("[{}] {}", q.header, q.question));
            if let Some(answer) = output.answers.get(&q.question) {
                for label in answer.split(',') {
                    if let Some(opt) = q.options.iter().find(|o| o.label == label) {
                        lines.push(format!("  ✓ {} — {}", opt.label, opt.description));
                    }
                }
            }
        }
        lines.push(String::new());
        lines.push("[answered]".to_string());
        msg.content = lines.join("\n");
    }
    let summary: Vec<String> = output
        .answers
        .iter()
        .map(|(q, a)| format!("{q}: {a}"))
        .collect();
    let _ = send_tx.send(OutboundControl::ResolveQuestion { prompt_id, output });
    if summary.is_empty() {
        state.messages.push(ChatMessage::new(
            MessageRole::System,
            "question answered (no selection)".to_string(),
        ));
    } else {
        state.messages.push(ChatMessage::new(
            MessageRole::System,
            format!("answered: {}", summary.join("; ")),
        ));
    }
}

pub(crate) fn handle_mouse_event(state: &mut AppState, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.scroll = state.scroll.saturating_sub(3);
        }
        MouseEventKind::ScrollDown => {
            state.scroll = state.scroll.saturating_add(3);
        }
        MouseEventKind::Down(_) => {
            let pos = ratatui::layout::Position::new(mouse.column, mouse.row);
            // Check think-block hitboxes first; clicks on thinking hints toggle
            // expansion and do not start a selection.
            let mut hit_think = false;
            for (rect, msg_idx, think_idx) in &state.think_hitboxes {
                if rect.contains(pos) {
                    let key = (*msg_idx, *think_idx);
                    if state.expanded_thinks.contains(&key) {
                        state.expanded_thinks.remove(&key);
                    } else {
                        state.expanded_thinks.insert(key);
                    }
                    hit_think = true;
                    break;
                }
            }
            if !hit_think {
                state.selection = None;
                if let Some(cursor) =
                    position_to_cursor(pos, &state.message_rects, &state.render_cache)
                {
                    state.selection = Some(Selection::new(cursor, cursor));
                    state.selecting = true;
                }
            }
        }
        MouseEventKind::Drag(_) => {
            if state.selecting {
                let pos = ratatui::layout::Position::new(mouse.column, mouse.row);
                if let Some(cursor) =
                    position_to_cursor(pos, &state.message_rects, &state.render_cache)
                {
                    if let Some(ref mut sel) = state.selection {
                        sel.head = cursor;
                    }
                }
            }
        }
        MouseEventKind::Up(_) => {
            state.selecting = false;
        }
        _ => {}
    }
}

pub(crate) fn handle_ws_event(
    state: &mut AppState,
    msg: serde_json::Value,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    match msg.get("type").and_then(|v| v.as_str()) {
        Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some("question") => {
            let payload = msg.get("payload").cloned().unwrap_or(json!({}));
            let prompt_id = payload
                .get("promptId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let questions: Vec<legion_runtime::AskUserQuestion> = payload
                .get("questions")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            if !questions.is_empty() {
                let message_index = state.messages.len();
                let pq = PendingQuestion {
                    prompt_id,
                    questions,
                    current: 0,
                    selected_labels: HashMap::new(),
                    focused: 0,
                    message_index,
                };
                let content = format_question_message(&pq);
                state
                    .messages
                    .push(ChatMessage::new(MessageRole::Question, content));
                state.pending_question = Some(pq);
            }
        }
        Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some("approval") => {
            // A Prompt/Required tool is waiting on the user; the status bar
            // renders the prompt and key input turns modal until answered.
            let payload = msg.get("payload").cloned().unwrap_or(json!({}));
            let prompt_id = payload
                .get("promptId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool = payload
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            state.pending_approval = Some((prompt_id, tool));
        }
        Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some("agent") => {
            if let Some(payload) = msg.get("payload") {
                match payload.get("stream").and_then(|v| v.as_str()) {
                    Some("todo_update") => {
                        if let Ok(items) = serde_json::from_value::<Vec<TodoItem>>(
                            payload.get("items").cloned().unwrap_or(json!([])),
                        ) {
                            let all_completed = !items.is_empty()
                                && items.iter().all(|t| t.status == TodoStatus::Completed);
                            state.todos = items;
                            state.todo_hide_at =
                                if all_completed && state.todo_auto_hide_seconds > 0 {
                                    Some(
                                        std::time::Instant::now()
                                            + std::time::Duration::from_secs(
                                                state.todo_auto_hide_seconds,
                                            ),
                                    )
                                } else {
                                    None
                                };
                        }
                    }
                    Some("lifecycle") => match payload.get("phase").and_then(|v| v.as_str()) {
                        Some("start") => {}
                        Some("end") => {
                            state.pending_request = false;
                            // A still-pending prompt at run end is stale (the
                            // gate timed out); drop it so keys work again.
                            state.pending_approval = None;
                            state.pending_question = None;
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant {
                                    last.state = MessageState::Done;
                                }
                            }
                            drain_queued_message(state, send_tx);
                        }
                        Some("error") => {
                            state.pending_request = false;
                            state.pending_approval = None;
                            state.pending_question = None;
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant {
                                    last.state = MessageState::Error;
                                }
                            }
                            // Surface the error text so a run that fails before
                            // producing any visible output does not look like a
                            // silent hang.
                            if let Some(err) = payload.get("error").and_then(|v| v.as_str()) {
                                let mut msg = ChatMessage::new(
                                    MessageRole::System,
                                    format!("run failed: {err}"),
                                );
                                msg.state = MessageState::Error;
                                state.messages.push(msg);
                            }
                            drain_queued_message(state, send_tx);
                        }
                        _ => {}
                    },
                    Some("assistant") => {
                        if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant
                                    && last.state != MessageState::Done
                                    && last.state != MessageState::Error
                                {
                                    if last.state == MessageState::Loading {
                                        last.state = MessageState::Streaming;
                                    }
                                    last.content.push_str(delta);
                                    return;
                                }
                            }
                            // Start a new assistant turn (e.g. after a tool call).
                            state.messages.push(ChatMessage {
                                role: MessageRole::Assistant,
                                content: delta.to_string(),
                                state: MessageState::Streaming,
                            });
                        }
                    }
                    Some("tool") => {
                        let name = payload
                            .get("tool_call")
                            .and_then(|t| t.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let arguments = payload
                            .get("tool_call")
                            .and_then(|t| t.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let tool_state = payload
                            .get("state")
                            .and_then(|v| v.as_str())
                            .unwrap_or("start");

                        if tool_state == "start" {
                            // The assistant turn ended by deciding to use a tool.
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant
                                    && (last.state == MessageState::Loading
                                        || last.state == MessageState::Streaming)
                                {
                                    last.state = MessageState::Done;
                                }
                            }
                            state.messages.push(ChatMessage {
                                role: MessageRole::Tool,
                                content: tool_card_json(tool_state, name, Some(arguments), None),
                                state: MessageState::Loading,
                            });
                        } else {
                            let result_content = payload
                                .get("result")
                                .and_then(|r| r.get("content"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let is_error = payload
                                .get("result")
                                .and_then(|r| r.get("is_error"))
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let new_state = if is_error {
                                MessageState::Error
                            } else {
                                MessageState::Done
                            };

                            // Update the most recent unfinished tool card.
                            if let Some(tool) = state.messages.iter_mut().rev().find(|m| {
                                m.role == MessageRole::Tool && m.state == MessageState::Loading
                            }) {
                                tool.content = tool_card_json(
                                    if is_error { "error" } else { "done" },
                                    name,
                                    Some(arguments),
                                    Some(result_content),
                                );
                                tool.state = new_state;
                            } else {
                                state.messages.push(ChatMessage {
                                    role: MessageRole::Tool,
                                    content: tool_card_json(
                                        if is_error { "error" } else { "done" },
                                        name,
                                        Some(arguments),
                                        Some(result_content),
                                    ),
                                    state: new_state,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("res") if msg.get("ok").and_then(|v| v.as_bool()) != Some(true) => {
            let err = msg
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("request failed");
            state
                .messages
                .push(ChatMessage::new(MessageRole::System, err.to_string()));
            state.messages.last_mut().unwrap().state = MessageState::Error;
        }
        _ => {}
    }
}

pub(crate) fn toggle_nearest_thinking(state: &mut AppState) {
    // Toggle the first thinking block in the most recent assistant message.
    for (msg_idx, msg) in state.messages.iter().enumerate().rev() {
        if msg.role != MessageRole::Assistant {
            continue;
        }
        let segments = parse_message_segments(&msg.content);
        if let Some(MessageSegment::Think { index, .. }) = segments
            .iter()
            .find(|s| matches!(s, MessageSegment::Think { .. }))
        {
            let key = (msg_idx, *index);
            if state.expanded_thinks.contains(&key) {
                state.expanded_thinks.remove(&key);
            } else {
                state.expanded_thinks.insert(key);
            }
            return;
        }
    }
}

/// Extract displayable chat messages from a `sessions.history` response.
///
/// Only user/assistant turns with non-empty content are kept: tool calls and
/// results add noise without helping the user re-read the conversation.
pub(crate) fn history_messages_from_payload(resp: &serde_json::Value) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    if let Some(messages) = resp
        .get("payload")
        .and_then(|p| p.get("messages"))
        .and_then(|m| m.as_array())
    {
        for msg in messages {
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                continue;
            }
            let role = match msg.get("role").and_then(|v| v.as_str()) {
                Some("user") => MessageRole::User,
                Some("assistant") => MessageRole::Assistant,
                _ => continue,
            };
            out.push(ChatMessage::new(role, content.to_string()));
        }
    }
    out
}
