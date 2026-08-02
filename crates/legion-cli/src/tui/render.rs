//! TUI rendering and layout.

use crate::tui::input::{apply_scroll, char_width, wrap_display_line};
use crate::tui::selection::{highlight_line_selection, line_selection_range};
use crate::tui::state::{AppState, ChatMessage, RenderKey, RenderedMessage, ThinkHint, ToolHint};
use crate::tui::widgets::{render_queue_panel, render_todo_panel, status_bar_lines};
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

    let str_width = |s: &str| crate::tui::input::visible_width(s);
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
            // Span is wider than the viewport: split it into display units.
            // Escape sequences (OSC 8 links) move as atomic zero-width units
            // so a sequence is never torn across lines.
            let span_style = span.style;
            let mut piece = String::new();
            let mut piece_width = 0usize;
            let mut rest = span.content.as_ref();
            while !rest.is_empty() {
                let (unit, tail) = crate::tui::input::next_display_unit(rest);
                rest = tail;
                let cw = if unit.starts_with('\x1b') {
                    0
                } else {
                    unit.chars().map(char_width).sum::<usize>()
                };
                if piece_width + cw > width && !piece.is_empty() {
                    result.push(
                        Line::from(vec![Span::styled(std::mem::take(&mut piece), span_style)])
                            .style(line_style),
                    );
                    piece_width = 0;
                }
                piece.push_str(unit);
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
    expanded_tools: &HashSet<usize>,
    width: u16,
) -> RenderKey {
    use std::hash::{Hash, Hasher};
    let mut content_hasher = std::collections::hash_map::DefaultHasher::new();
    msg.content.hash(&mut content_hasher);
    // Fold the tool-card expand state for this message into the same expanded
    // hash: a tool card re-renders (title-only ↔ full body) when its membership
    // in `expanded_tools` flips.
    let tool_expanded = expanded_tools.contains(&msg_index);
    let mut expanded_idxs: Vec<usize> = expanded
        .iter()
        .filter(|(m, _)| *m == msg_index)
        .map(|(_, t)| *t)
        .collect();
    expanded_idxs.sort_unstable();
    let mut expanded_hasher = std::collections::hash_map::DefaultHasher::new();
    expanded_idxs.hash(&mut expanded_hasher);
    tool_expanded.hash(&mut expanded_hasher);
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
) -> (Vec<Line<'static>>, Vec<ThinkHint>, Option<ToolHint>) {
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
    // A tool card's title is always a single row; remap its line index into
    // wrapped space. There is no line_count to recompute (the title does not
    // wrap into the body).
    let tool_hint = rendered.tool_hint.map(|hint| ToolHint {
        start_line: wrapped_start[hint.start_line],
    });
    (lines, think_hints, tool_hint)
}

/// Vertical layout plan for the main screen, as row heights per region
/// (top to bottom: chat, todo panel, queue panel, input, status bar).
///
/// Invariants, in priority order (highest first):
/// 1. The input box is always fully visible: `input` is the composer's
///    content lines plus 2 border rows, clamped to 3..=10 (taller content
///    scrolls inside the box), capped only by the chat minimum below.
///    Nothing may overlap it.
/// 2. The chat keeps at least 1 row whenever the terminal is >= 5 rows tall.
/// 3. The status bar shrinks from 2 rows (status + shortcut hints) to 1,
///    then disappears entirely before the chat minimum is touched.
/// 4. Panels yield before the status bar: the todo panel is sacrificed
///    first, then the queue panel. A panel is hidden entirely when showing
///    it would push the chat below 5 rows.
/// 5. Floating popups (slash completion, history search) are drawn by the
///    caller strictly above the input area, never over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutPlan {
    pub chat: u16,
    pub todo: u16,
    pub queue: u16,
    pub input: u16,
    pub status: u16,
}

pub(crate) fn plan_layout(
    total_height: u16,
    input_lines: usize,
    todos: usize,
    todo_max_display: usize,
    queued_messages: usize,
) -> LayoutPlan {
    let h = total_height;
    // 1. Input first: non-negotiable, capped only by the rows the chat
    // minimum below must keep.
    let chat_min = if h >= 5 { 1 } else { 0 };
    let input = (input_lines.max(1) as u16 + 2)
        .clamp(3, 10)
        .min(h.saturating_sub(chat_min));
    let mut remaining = h - input;
    // 2. Chat minimum outranks everything below it.
    let chat_min = chat_min.min(remaining);
    remaining -= chat_min;
    // 3. Status bar: 2 rows on tall terminals, 1 otherwise, 0 when squeezed.
    let status_want = if h >= 15 { 2 } else { 1 };
    let status = status_want.min(remaining);
    remaining -= status;
    // 4. Panels, lowest priority. A panel keeps its desired height only when
    // the chat still gets at least 5 rows with it shown.
    let queue = if queued_messages == 0 {
        0
    } else {
        // One row per visible item, plus a hint footer and the border (2);
        // capped to a quarter of the viewport.
        let desired = (queued_messages as u16 + 3).min(h / 4).max(3);
        let chat_left = h.saturating_sub(input + status + desired);
        if chat_left >= 5 {
            desired.min(remaining)
        } else {
            0
        }
    };
    remaining -= queue;
    let todo = if todos == 0 || h < 12 {
        0
    } else {
        // Capped by max_display and a third of the viewport.
        let desired = (todo_max_display.min(10) as u16 + 2).min(h / 3).max(3);
        let chat_left = h.saturating_sub(input + status + queue + desired);
        if chat_left >= 5 {
            desired.min(remaining)
        } else {
            0
        }
    };
    remaining -= todo;
    let chat = chat_min + remaining;
    LayoutPlan {
        chat,
        todo,
        queue,
        input,
        status,
    }
}

pub(crate) fn draw_ui(f: &mut ratatui::Frame, state: &mut AppState) {
    let theme = state.theme;
    state.viewport_height = f.area().height;

    let input_width = f.area().width.saturating_sub(2) as usize;
    let input_line_count: usize = state
        .composer
        .lines()
        .iter()
        .map(|line| wrap_display_line(line, input_width).len())
        .sum::<usize>()
        .max(1);
    let plan = plan_layout(
        f.area().height,
        input_line_count,
        state.todos.len(),
        state.todo_max_display,
        state.queued_messages.len(),
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(plan.chat),
            Constraint::Length(plan.todo),
            Constraint::Length(plan.queue),
            Constraint::Length(plan.input),
            Constraint::Length(plan.status),
        ])
        .split(f.area());

    let chat_area = chunks[0];
    let todo_area = chunks[1];
    let queue_area = chunks[2];
    let input_area = chunks[3];
    let status_area = chunks[4];
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
    state.tool_hitboxes.clear();
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
            // Tool-card title row: a single-line hitbox that toggles the card's
            // expand state. Mirrors the think-hint computation above.
            if let Some(hint) = &entry.tool_hint {
                let global_start = offset + hint.start_line;
                if global_start < window_end && global_start + 1 > state.scroll {
                    let start_y = inner_chat.y + global_start.saturating_sub(state.scroll) as u16;
                    let rect = Rect::new(inner_chat.x, start_y, inner_chat.width, 1);
                    state.tool_hitboxes.push((rect, msg_idx));
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
    if plan.todo > 0 && !state.todos.is_empty() {
        let todo_lines =
            render_todo_panel(state, todo_area.width.saturating_sub(2) as usize, &theme);
        let todo = Paragraph::new(Text::from(todo_lines))
            .block(Block::default().title("Tasks").borders(Borders::ALL));
        f.render_widget(todo, todo_area);
    }

    // Queue panel: the list of messages waiting behind the in-flight run,
    // with the selected item highlighted and a keybind hint footer.
    if plan.queue > 0 && !state.queued_messages.is_empty() {
        let queue_lines =
            render_queue_panel(state, queue_area.width.saturating_sub(2) as usize, &theme);
        let queue = Paragraph::new(Text::from(queue_lines))
            .block(Block::default().title("Queued").borders(Borders::ALL));
        f.render_widget(queue, queue_area);
    }

    // Input box.
    // The composer renders its own reversed-style cursor; the terminal cursor
    // is intentionally hidden by not calling set_cursor_position.
    let (input_title, border_color) = if state.composer.join().starts_with('!') {
        ("shell mode", theme.system_bar)
    } else {
        ("Input", theme.input_border)
    };
    state.composer.set_chrome(input_title, border_color);
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

    // History search popup. Floating layers must never cover the input box,
    // so the popup is centered in the rows above it and skipped when there
    // is no room.
    if let Some(ref hs) = state.history_search {
        let filtered = hs.filtered(state.input_history.entries());
        let region_height = input_area.y - f.area().y;
        if region_height >= 3 {
            let height = (filtered.len() as u16 + 4)
                .min(f.area().height / 2)
                .max(3)
                .min(region_height);
            let width = (f.area().width * 4 / 5).max(20);
            let x = f.area().x + (f.area().width - width) / 2;
            let y = f.area().y + (region_height - height) / 2;
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
            let list =
                List::new(items).block(Block::default().title("history").borders(Borders::ALL));
            f.render_widget(Clear, area);
            f.render_widget(list, area);
        }
    }

    // Status bar. A pending tool-approval prompt takes precedence over the
    // usual status so the user always sees what is blocking the run.
    if plan.status > 0 {
        let status_lines = status_bar_lines(state, &theme, plan.status);
        let status =
            Paragraph::new(Text::from(status_lines)).style(Style::default().bg(theme.status_bg));
        f.render_widget(status, status_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariants that must hold for every terminal height:
    /// regions tile the screen exactly, and the input box always gets its
    /// full clamped height (the "input is always fully visible" contract).
    fn assert_invariants(plan: LayoutPlan, h: u16, input_lines: usize) {
        assert_eq!(
            plan.chat + plan.todo + plan.queue + plan.input + plan.status,
            h,
            "regions must tile the screen exactly; plan: {plan:?}"
        );
        let want_input = (input_lines.max(1) as u16 + 2)
            .clamp(3, 10)
            .min(h.saturating_sub(if h >= 5 { 1 } else { 0 }));
        assert_eq!(
            plan.input, want_input,
            "input box must always get its full height; plan: {plan:?}"
        );
        if h >= 5 {
            assert!(plan.chat >= 1, "chat keeps at least 1 row; plan: {plan:?}");
        }
    }

    #[test]
    fn invariants_hold_across_heights() {
        for h in [3, 4, 5, 6, 8, 10, 12, 15, 20, 30, 50] {
            for input_lines in [1, 5, 9, 30] {
                // Empty panels.
                assert_invariants(plan_layout(h, input_lines, 0, 10, 0), h, input_lines);
                // Both panels populated.
                assert_invariants(plan_layout(h, input_lines, 7, 10, 2), h, input_lines);
            }
        }
    }

    #[test]
    fn input_grows_with_content_up_to_cap() {
        assert_eq!(plan_layout(30, 1, 0, 10, 0).input, 3);
        assert_eq!(plan_layout(30, 4, 0, 10, 0).input, 6);
        // 30 content lines still cap at 10 rows; the rest scrolls inside.
        assert_eq!(plan_layout(30, 30, 0, 10, 0).input, 10);
    }

    #[test]
    fn status_two_rows_on_tall_terminals_one_on_short() {
        assert_eq!(plan_layout(15, 1, 0, 10, 0).status, 2);
        assert_eq!(plan_layout(14, 1, 0, 10, 0).status, 1);
    }

    #[test]
    fn panels_visible_when_room_allows() {
        let plan = plan_layout(30, 1, 7, 10, 2);
        assert!(plan.todo > 0, "todo panel should show; plan: {plan:?}");
        assert!(plan.queue > 0, "queue panel should show; plan: {plan:?}");
        assert_eq!(plan.status, 2);
        assert!(plan.chat >= 5);
    }

    #[test]
    fn todo_sacrificed_before_queue() {
        // h=15, input=3, status=2: room for the queue panel (3 rows) and a
        // 7-row chat, but showing the todo panel too would drop chat below 5.
        let plan = plan_layout(15, 1, 7, 10, 2);
        assert_eq!(plan.queue, 3, "queue panel survives; plan: {plan:?}");
        assert_eq!(
            plan.todo, 0,
            "todo panel is sacrificed first; plan: {plan:?}"
        );
        assert_eq!(plan.status, 2);
        assert!(plan.chat >= 5);
    }

    #[test]
    fn panels_and_hints_yield_before_status_and_chat_minimum() {
        // h=8: panels hidden, status down to 1 row, chat keeps the rest.
        let plan = plan_layout(8, 1, 7, 10, 2);
        assert_eq!((plan.todo, plan.queue), (0, 0));
        assert_eq!(plan.status, 1);
        assert_eq!(plan.chat, 4);
    }

    #[test]
    fn extreme_heights_keep_input_intact() {
        // h=5: input 3, status 1, chat exactly 1.
        let plan = plan_layout(5, 1, 7, 10, 2);
        assert_eq!(
            plan,
            LayoutPlan {
                chat: 1,
                todo: 0,
                queue: 0,
                input: 3,
                status: 1,
            }
        );
        // h=4: below the chat-minimum threshold the chat drops to 0; the
        // status bar takes the one leftover row.
        let plan = plan_layout(4, 1, 0, 10, 0);
        assert_eq!(plan.input, 3);
        assert_eq!(plan.status, 1);
        assert_eq!(plan.chat, 0);
        // h=3: the input box is all that fits.
        let plan = plan_layout(3, 1, 0, 10, 0);
        assert_eq!(plan.input, 3);
        assert_eq!(plan.status, 0);
        assert_eq!(plan.chat, 0);
    }

    #[test]
    fn tall_input_pushes_panels_out_first() {
        // A 10-row input on a 15-row terminal: panels hidden, chat still
        // gets 3 rows (>= its 1-row minimum) alongside the 2-row status bar.
        let plan = plan_layout(15, 30, 7, 10, 2);
        assert_eq!(plan.input, 10);
        assert_eq!((plan.todo, plan.queue), (0, 0));
        assert_eq!(plan.status, 2);
        assert_eq!(plan.chat, 3);
    }
}
