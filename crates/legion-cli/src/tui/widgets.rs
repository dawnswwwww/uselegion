//! Shared TUI widgets and role-aware styling helpers.

use crate::tui::input::char_width;
use crate::tui::markdown::{markdown_lines, plain_lines};
use crate::tui::state::{
    AppState, ChatMessage, MessageRole, MessageState, PendingQuestion, RenderedMessage,
    SUBMIT_LABEL, ThinkHint,
};
use crate::tui::syntax::Highlighter;
use crate::tui::theme::Theme;
use crate::tui::tool_card::render_tool_card;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashSet;

/// Braille spinner frames, indexed by `AppState::spinner_frame`.
pub(crate) const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub(crate) fn role_color(role: MessageRole, theme: &Theme) -> Color {
    match role {
        MessageRole::User => theme.user_bar,
        MessageRole::Assistant => theme.assistant_bar,
        MessageRole::System => theme.system_bar,
        MessageRole::Tool => theme.tool_bar,
        MessageRole::Question => theme.question_bar,
    }
}

/// Background tint applied to each line of a message to visually group it.
pub(crate) fn role_background(role: MessageRole, theme: &Theme) -> Color {
    match role {
        MessageRole::User => theme.user_bg,
        MessageRole::Assistant => theme.assistant_bg,
        MessageRole::System => theme.system_bg,
        MessageRole::Tool => Color::Reset,
        MessageRole::Question => theme.question_bg,
    }
}

/// Left edge color bar for a message line.
pub(crate) fn left_bar_span(role: MessageRole, theme: &Theme) -> Span<'static> {
    Span::styled("█ ", Style::default().fg(role_color(role, theme)))
}

pub(crate) fn state_indicator(
    role: MessageRole,
    state: MessageState,
    theme: &Theme,
) -> Span<'static> {
    let (symbol, color) = match role {
        MessageRole::User => ("▸", theme.user_bar),
        MessageRole::System => ("!", theme.system_bar),
        MessageRole::Tool => ("◆", theme.tool_bar),
        MessageRole::Question => ("?", theme.question_bar),
        MessageRole::Assistant => match state {
            MessageState::Loading => ("◐", theme.system_bar),
            MessageState::Streaming => ("◐", theme.spinner_fg),
            MessageState::Done => ("●", theme.assistant_bar),
            MessageState::Error => ("✕", theme.error_fg),
        },
    };
    Span::styled(symbol, Style::default().fg(color))
}

pub(crate) fn role_label(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "You",
        MessageRole::Assistant => "Legion",
        MessageRole::System => "System",
        MessageRole::Tool => "tool",
        MessageRole::Question => "Question",
    }
}

pub(crate) fn prefix_spans(
    role: MessageRole,
    state: MessageState,
    theme: &Theme,
) -> Vec<Span<'static>> {
    vec![
        state_indicator(role, state, theme),
        Span::raw(" "),
        Span::styled(
            format!("{}:", role_label(role)),
            Style::default()
                .fg(role_color(role, theme))
                .add_modifier(Modifier::BOLD),
        ),
    ]
}

pub(crate) fn prepend_prefix(lines: &mut Vec<Line<'static>>, prefix: Vec<Span<'static>>) {
    if let Some(first) = lines.first_mut() {
        for span in prefix.into_iter().rev() {
            first.spans.insert(0, span);
        }
    } else {
        lines.push(Line::from(prefix));
    }
}

/// A segment of rendered message content.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MessageSegment<'a> {
    Text(&'a str),
    Think { index: usize, text: &'a str },
}

/// Parse message content into normal text and `<think>` reasoning segments.
///
/// Unmatched opening `<think>` tags cause all following content to be treated as
/// reasoning until a matching `</think>` is seen. Each think block is assigned an
/// increasing index so the UI can expand/collapse individual blocks.
pub(crate) fn parse_message_segments(content: &str) -> Vec<MessageSegment<'_>> {
    let mut segments = Vec::new();
    let mut rest = content;
    let mut in_think = false;
    let mut think_index = 0;

    while !rest.is_empty() {
        let tag = if in_think { "</think>" } else { "<think>" };
        match rest.find(tag) {
            Some(idx) => {
                let before = &rest[..idx];
                if !before.is_empty() {
                    segments.push(if in_think {
                        MessageSegment::Think {
                            index: think_index,
                            text: before,
                        }
                    } else {
                        MessageSegment::Text(before)
                    });
                }
                rest = &rest[idx + tag.len()..];
                if in_think {
                    think_index += 1;
                }
                in_think = !in_think;
            }
            None => {
                segments.push(if in_think {
                    MessageSegment::Think {
                        index: think_index,
                        text: rest,
                    }
                } else {
                    MessageSegment::Text(rest)
                });
                break;
            }
        }
    }
    segments
}

pub(crate) fn message_lines(
    msg: &ChatMessage,
    msg_index: usize,
    expanded: &HashSet<(usize, usize)>,
    _viewport_width: u16,
    theme: &Theme,
    highlighter: &Highlighter,
) -> RenderedMessage {
    if msg.role == MessageRole::Tool {
        let mut lines = render_tool_card(&msg.content, theme);
        if msg.state == MessageState::Streaming || msg.state == MessageState::Loading {
            lines.push(Line::from(Span::styled(
                "▌",
                Style::default().fg(theme.spinner_fg),
            )));
        }
        return RenderedMessage {
            lines,
            think_hints: Vec::new(),
        };
    }

    if msg.role == MessageRole::Question {
        // Question prompts are pre-formatted plain text; skip markdown parsing
        // so list markers and checkboxes render literally.
        let lines = plain_lines(&msg.content);
        return RenderedMessage {
            lines,
            think_hints: Vec::new(),
        };
    }

    let prefix = prefix_spans(msg.role, msg.state, theme);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut think_hints: Vec<ThinkHint> = Vec::new();
    let mut first = true;

    for segment in parse_message_segments(&msg.content) {
        match segment {
            MessageSegment::Text(text) => {
                // While tokens are still streaming in, render plain text:
                // partial markdown syntax (an unclosed fence, a half-typed
                // `**`) would flicker, and re-parsing the growing message
                // every frame is wasted work. The full markdown render
                // happens once when the message reaches a terminal state.
                let mut md =
                    if msg.state == MessageState::Streaming || msg.state == MessageState::Loading {
                        plain_lines(text)
                    } else {
                        markdown_lines(text, theme, highlighter)
                    };
                if first {
                    prepend_prefix(&mut md, prefix.clone());
                    first = false;
                }
                lines.extend(md);
            }
            MessageSegment::Think { index, text } => {
                if text.is_empty() {
                    continue;
                }
                let key = (msg_index, index);
                let is_expanded = expanded.contains(&key);
                let hint_line = lines.len();
                let hint_symbol = if is_expanded { "▼" } else { "▶" };
                let think_prefix = Span::styled(
                    format!("[thinking] {}", hint_symbol),
                    Style::default()
                        .fg(theme.tool_bar)
                        .add_modifier(Modifier::ITALIC),
                );
                let mut hint_line_spans = prefix.clone();
                hint_line_spans.push(think_prefix);
                if first {
                    lines.push(Line::from(hint_line_spans));
                    first = false;
                } else {
                    lines.push(Line::from(vec![Span::styled(
                        format!("[thinking] {}", hint_symbol),
                        Style::default()
                            .fg(theme.tool_bar)
                            .add_modifier(Modifier::ITALIC),
                    )]));
                }

                if is_expanded {
                    let content_lines = text.split('\n').map(|l| {
                        Line::from(Span::styled(
                            l.to_string(),
                            Style::default()
                                .fg(theme.tool_bar)
                                .add_modifier(Modifier::ITALIC),
                        ))
                    });
                    let content_count = text.split('\n').count();
                    lines.extend(content_lines);
                    think_hints.push(ThinkHint {
                        block_index: index,
                        start_line: hint_line,
                        line_count: 1 + content_count,
                    });
                } else {
                    think_hints.push(ThinkHint {
                        block_index: index,
                        start_line: hint_line,
                        line_count: 1,
                    });
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(prefix));
    }

    if msg.state == MessageState::Streaming || msg.state == MessageState::Loading {
        lines.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(theme.spinner_fg),
        )));
    }

    RenderedMessage { lines, think_hints }
}

/// Truncate a string to fit within `width` display columns, adding an ellipsis
/// when truncation occurs.
pub(crate) fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut current_width = 0usize;
    for c in text.chars() {
        let w = char_width(c);
        if current_width + w > width && !result.is_empty() {
            // Back up and append an ellipsis if there is room.
            if width >= 1 {
                while current_width > width.saturating_sub(1) && !result.is_empty() {
                    let removed = result.pop().unwrap_or('\0');
                    current_width = current_width.saturating_sub(char_width(removed));
                }
                result.push('…');
            }
            return result;
        }
        result.push(c);
        current_width += w;
    }
    result
}

/// Render the todo checklist for the Tasks panel.
///
/// Items are prioritized: in-progress first, then pending, then completed.
/// Each line is truncated to fit the panel width. If there are more items
/// than `todo_max_display`, the last line shows a summary count.
pub(crate) fn render_todo_panel(
    state: &AppState,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut ordered: Vec<&legion_runtime::TodoItem> = state.todos.iter().collect();
    ordered.sort_by_key(|t| match t.status {
        legion_runtime::TodoStatus::InProgress => 0,
        legion_runtime::TodoStatus::Pending => 1,
        legion_runtime::TodoStatus::Completed => 2,
    });

    let max_lines = state.todo_max_display.max(1);
    let visible: Vec<&legion_runtime::TodoItem> = ordered.iter().take(max_lines).copied().collect();
    let hidden = ordered.len().saturating_sub(max_lines);

    let mut lines = Vec::with_capacity(visible.len() + usize::from(hidden > 0));
    for item in visible {
        let (icon, icon_color, text_style) = match item.status {
            legion_runtime::TodoStatus::Pending => {
                ("□", theme.tool_bar, Style::default().fg(theme.status_fg))
            }
            legion_runtime::TodoStatus::InProgress => (
                "■",
                theme.system_bar,
                Style::default()
                    .fg(theme.system_bar)
                    .add_modifier(Modifier::BOLD),
            ),
            legion_runtime::TodoStatus::Completed => (
                "✓",
                theme.assistant_bar,
                Style::default()
                    .fg(theme.tool_bar)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
        };

        let mut text = item.content.clone();
        if item.status == legion_runtime::TodoStatus::InProgress && !item.active_form.is_empty() {
            text.push_str(&format!(" — {}", item.active_form));
        }
        text = truncate_to_width(&text, width.saturating_sub(2));

        lines.push(Line::from(vec![
            Span::styled(format!("{} ", icon), Style::default().fg(icon_color)),
            Span::styled(text, text_style),
        ]));
    }

    if hidden > 0 {
        let suffix = format!("+{} more", hidden);
        lines.push(Line::from(Span::styled(
            truncate_to_width(&suffix, width),
            Style::default().fg(theme.tool_bar),
        )));
    }

    lines
}

/// Build the status line(s) rendered at the bottom of the screen.
pub(crate) fn status_bar_lines(
    state: &AppState,
    theme: &Theme,
    status_height: u16,
) -> Vec<Line<'static>> {
    let (status_text, status_color) = if let Some(pq) = &state.pending_question {
        let hint = question_hint(pq);
        let header = pq
            .current_question()
            .map(|q| q.header.as_str())
            .unwrap_or(SUBMIT_LABEL);
        (format!("{} ({})", header, hint), theme.system_bar)
    } else if let Some((_, tool)) = &state.pending_approval {
        (format!("approve tool '{tool}'? y/n"), theme.system_bar)
    } else if state.is_active() {
        let frame = SPINNER[state.spinner_frame % SPINNER.len()];
        (
            format!("{frame} typing... (esc to cancel)"),
            theme.system_bar,
        )
    } else {
        (
            state.status.clone(),
            if state.status == "connected" {
                theme.assistant_bar
            } else {
                theme.system_bar
            },
        )
    };

    let yolo_hint = if state
        .messages
        .iter()
        .any(|m| m.role == MessageRole::System && m.content.contains("yolo mode"))
    {
        " · yolo"
    } else {
        ""
    };
    let peer_hint = if state.session_peer.is_empty() {
        String::new()
    } else {
        format!(" · {}", state.session_peer)
    };
    let goal_hint = state.goal.as_ref().map(|g| {
        if g.status.is_active() {
            let truncated = if g.objective.chars().count() > 40 {
                format!("{}…", g.objective.chars().take(40).collect::<String>())
            } else {
                g.objective.clone()
            };
            format!(" · goal: {truncated}")
        } else {
            format!(
                " · goal: {} ({})",
                g.status,
                g.objective.chars().take(20).collect::<String>()
            )
        }
    });

    let status_line = Line::from(vec![
        Span::raw("status: "),
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(yolo_hint, Style::default().fg(theme.error_fg)),
        Span::styled(peer_hint, Style::default().fg(theme.tool_bar)),
        Span::styled(
            goal_hint.unwrap_or_default(),
            Style::default().fg(theme.user_bar),
        ),
    ]);

    let shortcuts_line = Line::from(vec![
        Span::styled("^Q ", Style::default().fg(theme.tool_bar)),
        Span::raw("quit "),
        Span::styled("Enter ", Style::default().fg(theme.tool_bar)),
        Span::raw("send "),
        Span::styled("Alt+Enter ", Style::default().fg(theme.tool_bar)),
        Span::raw("newline "),
        Span::styled("Esc ", Style::default().fg(theme.tool_bar)),
        Span::raw("cancel "),
        Span::styled("↑/↓ ", Style::default().fg(theme.tool_bar)),
        Span::raw("history "),
        Span::styled("PgUp/PgDn ", Style::default().fg(theme.tool_bar)),
        Span::raw("scroll "),
        Span::styled("/ ", Style::default().fg(theme.tool_bar)),
        Span::raw("commands "),
        Span::styled("Tab ", Style::default().fg(theme.tool_bar)),
        Span::raw("complete"),
    ]);

    if status_height == 2 {
        vec![status_line, shortcuts_line]
    } else {
        vec![status_line]
    }
}

fn question_hint(pq: &PendingQuestion) -> String {
    if pq.is_submit_tab() {
        "enter=submit · esc=cancel".to_string()
    } else if pq.is_multi_select() {
        "←/→ tab · ↑/↓ option · space=toggle · enter=select · esc".to_string()
    } else {
        "←/→ tab · ↑/↓ option · enter=select · esc".to_string()
    }
}
