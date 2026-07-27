//! TUI rendering and layout.

use crate::tui::input::{apply_scroll, char_width, input_visual_lines};
use crate::tui::selection::{highlight_line_selection, line_selection_range};
use crate::tui::state::{AppState, ChatMessage, RenderKey, RenderedMessage, ThinkHint};
use crate::tui::widgets::{render_todo_panel, status_bar_lines};
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
};
use std::collections::HashSet;

/// Wrap a single `Line` into multiple lines so that each fits within `width`
/// terminal columns. Preserves span styles where possible.
pub(crate) fn wrap_line_to_width(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    if width == 0 {
        return vec![line];
    }

    let str_width = |s: &str| s.chars().map(char_width).sum::<usize>();
    let spans_width =
        |spans: &[Span<'static>]| spans.iter().map(|s| str_width(&s.content)).sum::<usize>();
    if spans_width(&line.spans) <= width {
        return vec![line];
    }

    let mut result = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    let line_style = line.style;
    let spans = line.spans;

    for span in spans {
        let span_width = str_width(&span.content);

        if current_width + span_width <= width {
            current_spans.push(span);
            current_width += span_width;
            continue;
        }

        // Span doesn't fit entirely.
        if current_width > 0 {
            // Flush what we have so far.
            result.push(Line::from(std::mem::take(&mut current_spans)).style(line_style));
            current_width = 0;
        }

        if span_width <= width {
            // The whole span fits on a fresh line.
            current_spans.push(span);
            current_width = span_width;
        } else {
            // Span is wider than the viewport: split it character-by-character.
            let span_style = span.style;
            let mut piece = String::new();
            let mut piece_width = 0usize;
            for c in span.content.chars() {
                let cw = char_width(c);
                if piece_width + cw > width && !piece.is_empty() {
                    result.push(
                        Line::from(vec![Span::styled(std::mem::take(&mut piece), span_style)])
                            .style(line_style),
                    );
                    piece_width = 0;
                }
                piece.push(c);
                piece_width += cw;
            }
            if !piece.is_empty() {
                current_spans.push(Span::styled(piece, span_style));
                current_width = piece_width;
            }
        }
    }

    if !current_spans.is_empty() {
        result.push(Line::from(current_spans).style(line_style));
    }

    if result.is_empty() {
        result.push(Line::from("").style(line_style));
    }

    result
}

pub(crate) fn render_key(
    msg: &ChatMessage,
    msg_index: usize,
    expanded: &HashSet<(usize, usize)>,
    width: u16,
) -> RenderKey {
    use std::hash::{Hash, Hasher};
    let mut content_hasher = std::collections::hash_map::DefaultHasher::new();
    msg.content.hash(&mut content_hasher);
    let mut expanded_idxs: Vec<usize> = expanded
        .iter()
        .filter(|(m, _)| *m == msg_index)
        .map(|(_, t)| *t)
        .collect();
    expanded_idxs.sort_unstable();
    let mut expanded_hasher = std::collections::hash_map::DefaultHasher::new();
    expanded_idxs.hash(&mut expanded_hasher);
    RenderKey {
        content_hash: content_hasher.finish(),
        state: msg.state,
        expanded_hash: expanded_hasher.finish(),
        width,
    }
}

/// Wrap rendered lines to the viewport width and translate thinking-hint
/// line numbers (which index the unwrapped lines) into wrapped-line space.
pub(crate) fn wrap_and_remap(
    rendered: RenderedMessage,
    width: u16,
) -> (Vec<Line<'static>>, Vec<ThinkHint>) {
    let mut lines = Vec::new();
    let mut wrapped_start = Vec::with_capacity(rendered.lines.len());
    for line in rendered.lines {
        wrapped_start.push(lines.len());
        lines.extend(wrap_line_to_width(line, width));
    }
    let total = lines.len();
    let think_hints = rendered
        .think_hints
        .iter()
        .map(|hint| {
            let start = wrapped_start[hint.start_line];
            let end_orig = hint.start_line + hint.line_count;
            let end = if end_orig < wrapped_start.len() {
                wrapped_start[end_orig]
            } else {
                total
            };
            ThinkHint {
                block_index: hint.block_index,
                start_line: start,
                line_count: end.saturating_sub(start).max(1),
            }
        })
        .collect();
    (lines, think_hints)
}

pub(crate) fn draw_ui(f: &mut ratatui::Frame, state: &mut AppState) {
    let theme = state.theme;
    state.viewport_height = f.area().height;

    // Dynamic input area: grows with content up to a cap, but always leaves
    // room for the chat (min 5) and the status bar.
    let input_width = f.area().width.saturating_sub(2) as usize;
    let input_line_count = input_visual_lines(&state.composer.join(), input_width)
        .len()
        .max(1);
    let input_height = (input_line_count as u16 + 2).clamp(3, 10);
    // On very short terminals keep a compact single-line status bar; otherwise
    // split it into a status line plus a shortcuts line so it is readable.
    let status_height = if f.area().height >= 15 { 2 } else { 1 };

    // Todo panel height: capped by max_display and total height so the chat
    // always keeps at least 5 lines. Hide entirely when empty or short terminal.
    let max_todo_lines = state.todo_max_display.min(10);
    let todo_height = if state.todos.is_empty() || f.area().height < 12 {
        0
    } else {
        let desired = (max_todo_lines as u16 + 2).min(f.area().height / 3).max(3);
        let remaining_for_chat = f
            .area()
            .height
            .saturating_sub(desired + input_height + status_height);
        if remaining_for_chat < 5 { 0 } else { desired }
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(todo_height),
            Constraint::Length(input_height),
            Constraint::Length(status_height),
        ])
        .split(f.area());

    let chat_area = chunks[0];
    let todo_area = chunks[1];
    let input_area = chunks[2];
    state.chat_area = chat_area;
    state.input_area_width = input_area.width.saturating_sub(2);

    // The Paragraph widget renders inside the chat block's borders, so the
    // usable width is two columns narrower. Pass that to the cache so wrapped
    // lines fit exactly inside the inner area.
    let chat_inner_width = chat_area.width.saturating_sub(2);
    state.ensure_render_cache(chat_inner_width);
    let visible_chat_lines = chat_area.height.saturating_sub(2) as usize;
    let total_lines = state.cached_total_lines();
    let max_scroll = total_lines.saturating_sub(visible_chat_lines);
    state.visible_chat_lines = visible_chat_lines as u16;
    apply_scroll(state, max_scroll);

    // Single pass over the cached per-message renders: collect the visible
    // line window and the thinking-hint hitboxes together.
    state.think_hitboxes.clear();
    state.message_rects.clear();
    let inner_chat = chat_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let window_end = state.scroll + visible_chat_lines;
    let msg_count = state.messages.len();
    let mut visible_lines: Vec<Line> = Vec::new();
    let mut offset = 0usize;
    for (msg_idx, entry) in state.render_cache.iter().enumerate() {
        let Some(entry) = entry else { continue };
        let msg_lines = entry.lines.len();
        // One blank separator line between messages.
        let has_sep = msg_idx + 1 < msg_count;
        let msg_end = offset + msg_lines + usize::from(has_sep);

        if msg_end > state.scroll && offset < window_end {
            let local_start = state.scroll.saturating_sub(offset);
            let local_end = msg_lines.min(window_end - offset);
            for line_idx in local_start..local_end {
                let mut line = entry.lines[line_idx].clone();
                if let Some(ref sel) = state.selection {
                    if let Some((start, end)) = line_selection_range(sel, msg_idx, line_idx) {
                        let end = if end == usize::MAX {
                            line.spans.iter().map(|s| s.content.chars().count()).sum()
                        } else {
                            end
                        };
                        line = highlight_line_selection(line, start, end);
                    }
                }
                visible_lines.push(line);
            }
            let sep_pos = offset + msg_lines;
            if has_sep && sep_pos >= state.scroll && sep_pos < window_end {
                visible_lines.push(Line::from(""));
            }
            // Cache this message body's on-screen rectangle so clicks can
            // hit-test via `Rect::contains`. Body only — the separator gap is
            // deliberately excluded, so a click on the blank line between
            // messages maps to `None`.
            let body_start = offset.max(state.scroll);
            let body_end = (offset + msg_lines).min(window_end);
            if body_end > body_start {
                let start_y = inner_chat.y + (body_start - state.scroll) as u16;
                let height = (body_end - body_start) as u16;
                // first_line = body_start - offset: index into the message's
                // rendered lines of the row now at `start_y` (nonzero when the
                // message's top is scrolled out of view).
                let first_line = body_start - offset;
                state.message_rects.push((
                    msg_idx,
                    Rect::new(inner_chat.x, start_y, inner_chat.width, height),
                    first_line,
                ));
            }
            for hint in &entry.think_hints {
                let global_start = offset + hint.start_line;
                let global_end = global_start + hint.line_count;
                if global_start < window_end && global_end > state.scroll {
                    let start_y = inner_chat.y + global_start.saturating_sub(state.scroll) as u16;
                    let height = global_end.saturating_sub(state.scroll.max(global_start)) as u16;
                    let rect = Rect::new(inner_chat.x, start_y, inner_chat.width, height);
                    state.think_hitboxes.push((rect, msg_idx, hint.block_index));
                }
            }
        }

        offset = msg_end;
        if offset >= window_end {
            break;
        }
    }

    let mut chat_block = Block::default().title("Legion").borders(Borders::ALL);
    if state.scroll < max_scroll {
        chat_block = chat_block.title_bottom(
            Line::from(Span::styled(
                " ↓ more ",
                Style::default().fg(theme.system_bar),
            ))
            .right_aligned(),
        );
    }
    if !state.queued_messages.is_empty() {
        chat_block = chat_block.title_top(
            Line::from(Span::styled(
                format!(" ⏳ {} queued ", state.queued_messages.len()),
                Style::default().fg(theme.user_bar),
            ))
            .right_aligned(),
        );
    }
    let chat = Paragraph::new(Text::from(visible_lines)).block(chat_block);
    f.render_widget(chat, chat_area);

    let scrollbar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"));
    f.render_stateful_widget(
        scrollbar,
        chat_area.inner(Margin {
            horizontal: 0,
            vertical: 1,
        }),
        &mut ratatui::widgets::ScrollbarState::new(max_scroll).position(state.scroll),
    );

    // Todo panel.
    if todo_height > 0 && !state.todos.is_empty() {
        let todo_lines =
            render_todo_panel(state, todo_area.width.saturating_sub(2) as usize, &theme);
        let todo = Paragraph::new(Text::from(todo_lines))
            .block(Block::default().title("Tasks").borders(Borders::ALL));
        f.render_widget(todo, todo_area);
    }

    // Input box.
    // The composer renders its own reversed-style cursor; the terminal cursor
    // is intentionally hidden by not calling set_cursor_position.
    let input_title = if state.composer.join().starts_with('!') {
        "shell mode"
    } else {
        "Input"
    };
    state.composer.set_title(input_title);
    state.composer.render(input_area, f.buffer_mut());

    // Slash-command completion menu: a floating list above the input box,
    // open while the input is a bare `/name` (see AppState::slash_suggestions).
    let suggestions = state.slash_suggestions();
    if !suggestions.is_empty() {
        let height = (suggestions.len() as u16 + 2).min(input_area.y);
        // A terminal too short for a border plus one row just skips the menu.
        if height >= 3 {
            let area = Rect::new(
                input_area.x,
                input_area.y - height,
                input_area.width,
                height,
            );
            let items: Vec<ListItem> = suggestions
                .iter()
                .enumerate()
                .map(|(idx, cmd)| {
                    let aliases = if cmd.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ({})",
                            cmd.aliases
                                .iter()
                                .map(|alias| format!("/{alias}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let item = ListItem::new(Line::from(format!(
                        "/{}{} — {}",
                        cmd.name, aliases, cmd.description
                    )));
                    if idx == state.slash_selected {
                        item.style(Style::default().fg(theme.selected_fg).bg(theme.selected_bg))
                    } else {
                        item
                    }
                })
                .collect();
            let list =
                List::new(items).block(Block::default().title("commands").borders(Borders::ALL));
            f.render_widget(Clear, area);
            f.render_widget(list, area);
        }
    }

    // History search popup.
    if let Some(ref hs) = state.history_search {
        let filtered = hs.filtered(&state.input_history);
        let height = (filtered.len() as u16 + 4).min(f.area().height / 2).max(5);
        let width = (f.area().width * 4 / 5).max(20);
        let x = f.area().x + (f.area().width - width) / 2;
        let y = f.area().y + (f.area().height - height) / 2;
        let area = Rect::new(x, y, width, height);

        let items: Vec<ListItem> = filtered
            .iter()
            .enumerate()
            .map(|(idx, (_, text))| {
                let display = if text.len() > width as usize - 4 {
                    format!("{}…", &text[..width as usize - 5])
                } else {
                    text.to_string()
                };
                let item = ListItem::new(Line::from(display));
                if idx == hs.selected {
                    item.style(Style::default().fg(theme.selected_fg).bg(theme.selected_bg))
                } else {
                    item
                }
            })
            .collect();
        let list = List::new(items).block(Block::default().title("history").borders(Borders::ALL));
        f.render_widget(Clear, area);
        f.render_widget(list, area);
    }

    // Status bar. A pending tool-approval prompt takes precedence over the
    // usual status so the user always sees what is blocking the run.
    let status_lines = status_bar_lines(state, &theme, status_height);
    let status =
        Paragraph::new(Text::from(status_lines)).style(Style::default().bg(theme.status_bg));
    f.render_widget(status, chunks[3]);
}
