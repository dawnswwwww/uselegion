//! Scrollback text selection model and coordinate conversion.

use crate::tui::state::{CachedRender, ChatMessage};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// A cursor position inside the rendered scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub message_index: usize,
    /// Line index within the rendered (wrapped) lines of the message.
    pub line_index: usize,
    /// Character index within the line (UTF-8 char boundaries).
    pub char_index: usize,
}

impl Cursor {
    pub fn new(message_index: usize, line_index: usize, char_index: usize) -> Self {
        Self {
            message_index,
            line_index,
            char_index,
        }
    }
}

/// A user selection in the scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub anchor: Cursor,
    pub head: Cursor,
}

impl Selection {
    pub fn new(anchor: Cursor, head: Cursor) -> Self {
        Self { anchor, head }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Return (start, end) in document order.
    pub fn normalized(&self) -> (Cursor, Cursor) {
        let a = (
            self.anchor.message_index,
            self.anchor.line_index,
            self.anchor.char_index,
        );
        let b = (
            self.head.message_index,
            self.head.line_index,
            self.head.char_index,
        );
        if a <= b {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether a given char position falls inside the selection.
    #[allow(dead_code)]
    pub fn contains(&self, message_index: usize, line_index: usize, char_index: usize) -> bool {
        let (start, end) = self.normalized();
        let pos = (message_index, line_index, char_index);
        let start_pos = (start.message_index, start.line_index, start.char_index);
        let end_pos = (end.message_index, end.line_index, end.char_index);
        pos >= start_pos && pos < end_pos
    }
}

/// Convert a terminal mouse position inside the chat area to a cursor in the
/// rendered scrollback. Returns `None` when the click is outside rendered text.
pub(crate) fn position_to_cursor(
    pos: Position,
    chat_area: Rect,
    messages: &[ChatMessage],
    scroll_offset: usize,
    render_cache: &[Option<CachedRender>],
) -> Option<Cursor> {
    let row = pos.y.saturating_sub(chat_area.y) as usize;
    let target_line = scroll_offset + row;

    let mut current_line = 0usize;
    for (msg_index, _msg) in messages.iter().enumerate() {
        let rendered = render_cache.get(msg_index).and_then(|r| r.as_ref())?;
        let msg_total_lines = rendered.lines.len();
        if msg_total_lines == 0 {
            continue;
        }
        if current_line + msg_total_lines > target_line {
            let line_index = target_line - current_line;
            let line = rendered.lines.get(line_index)?;
            let text = line_to_text(line);
            let char_index = x_to_char_index(&text, pos.x, chat_area.x);
            return Some(Cursor::new(msg_index, line_index, char_index));
        }
        current_line += msg_total_lines;
    }
    None
}

/// Extract selected text as a single string.
pub(crate) fn selected_text(
    selection: &Selection,
    messages: &[ChatMessage],
    render_cache: &[Option<CachedRender>],
) -> String {
    let (start, end) = selection.normalized();
    let mut result = Vec::new();

    for msg_index in start.message_index..=end.message_index.min(messages.len().saturating_sub(1)) {
        let Some(rendered) = render_cache.get(msg_index).and_then(|r| r.as_ref()) else {
            continue;
        };
        let start_line = if msg_index == start.message_index {
            start.line_index
        } else {
            0
        };
        let end_line = if msg_index == end.message_index {
            end.line_index
        } else {
            rendered.lines.len().saturating_sub(1)
        };

        for line_index in start_line..=end_line {
            let Some(line) = rendered.lines.get(line_index) else {
                continue;
            };
            let text = line_to_text(line);
            let start_c = if msg_index == start.message_index && line_index == start.line_index {
                start.char_index.min(text.chars().count())
            } else {
                0
            };
            let end_c = if msg_index == end.message_index && line_index == end.line_index {
                end.char_index.min(text.chars().count())
            } else {
                text.chars().count()
            };
            let slice = char_slice(&text, start_c, end_c);
            result.push(slice);
        }
    }

    result.join("\n")
}

/// Convert a ratatui `Line` to a plain string.
fn line_to_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Convert an x coordinate (absolute terminal column) to a char index in
/// `text`. `area_x` is the left edge of the chat area.
fn x_to_char_index(text: &str, x: u16, area_x: u16) -> usize {
    let target = x.saturating_sub(area_x) as usize;
    let mut width = 0usize;
    for (idx, c) in text.chars().enumerate() {
        let cw = crate::tui::input::char_width(c);
        if width + cw > target {
            return idx;
        }
        width += cw;
    }
    text.chars().count()
}

/// Extract a slice of chars from `text` by char index range.
fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

/// Build an OSC 52 escape sequence that writes `text` to the system clipboard.
pub(crate) fn osc52_copy(text: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let encoded = STANDARD.encode(text);
    format!("\x1b]52;c;{}\x07", encoded)
}

/// Return the selected char range `[start, end)` for a specific rendered line,
/// or `None` if the line is not part of the selection.
pub(crate) fn line_selection_range(
    selection: &Selection,
    msg_idx: usize,
    line_idx: usize,
) -> Option<(usize, usize)> {
    let (start, end) = selection.normalized();
    if msg_idx < start.message_index || msg_idx > end.message_index {
        return None;
    }
    if msg_idx == start.message_index && msg_idx == end.message_index {
        if line_idx < start.line_index || line_idx > end.line_index {
            return None;
        }
        let line_start = if line_idx == start.line_index {
            start.char_index
        } else {
            0
        };
        let line_end = if line_idx == end.line_index {
            end.char_index
        } else {
            usize::MAX
        };
        return Some((line_start, line_end));
    }
    if msg_idx == start.message_index {
        if line_idx < start.line_index {
            return None;
        }
        let line_start = if line_idx == start.line_index {
            start.char_index
        } else {
            0
        };
        return Some((line_start, usize::MAX));
    }
    if msg_idx == end.message_index {
        if line_idx > end.line_index {
            return None;
        }
        let line_end = if line_idx == end.line_index {
            end.char_index
        } else {
            usize::MAX
        };
        return Some((0, line_end));
    }
    // Fully selected middle message.
    Some((0, usize::MAX))
}

/// Apply a selection highlight (REVERSED modifier) to the characters in `line`
/// whose char indices fall in `[start_char, end_char)`.
pub(crate) fn highlight_line_selection(
    line: Line<'static>,
    start_char: usize,
    end_char: usize,
) -> Line<'static> {
    let mut new_spans = Vec::new();
    let mut char_pos = 0usize;
    for span in line.spans {
        let span_chars = span.content.chars().count();
        let span_end = char_pos + span_chars;
        let sel_start = start_char.max(char_pos);
        let sel_end = end_char.min(span_end);

        if sel_start < sel_end {
            // Split the span into up to three parts: before, selected, after.
            let before_count = sel_start - char_pos;
            let selected_count = sel_end - sel_start;
            let after_count = span_end - sel_end;

            let mut chars = span.content.chars();
            if before_count > 0 {
                let before: String = chars.by_ref().take(before_count).collect();
                new_spans.push(Span::styled(before, span.style));
            }
            if selected_count > 0 {
                let selected: String = chars.by_ref().take(selected_count).collect();
                new_spans.push(Span::styled(
                    selected,
                    span.style.add_modifier(Modifier::REVERSED),
                ));
            }
            if after_count > 0 {
                let after: String = chars.collect();
                new_spans.push(Span::styled(after, span.style));
            }
        } else {
            new_spans.push(span);
        }
        char_pos = span_end;
    }
    Line::from(new_spans).style(line.style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::{Line, Span};

    #[test]
    fn selection_contains_inside() {
        let sel = Selection::new(Cursor::new(0, 0, 2), Cursor::new(0, 0, 5));
        assert!(sel.contains(0, 0, 2));
        assert!(sel.contains(0, 0, 4));
        assert!(!sel.contains(0, 0, 5));
        assert!(!sel.contains(0, 0, 1));
    }

    #[test]
    fn selection_normalized_swaps_backward() {
        let sel = Selection::new(Cursor::new(1, 0, 5), Cursor::new(0, 2, 3));
        let (start, end) = sel.normalized();
        assert_eq!(start.message_index, 0);
        assert_eq!(end.message_index, 1);
    }

    #[test]
    fn selected_text_extracts_range() {
        let messages = vec![ChatMessage::new(
            crate::tui::state::MessageRole::User,
            "hello\nworld",
        )];
        let rendered = CachedRender {
            key: crate::tui::state::RenderKey {
                content_hash: 0,
                state: crate::tui::state::MessageState::Done,
                expanded_hash: 0,
                width: 80,
            },
            lines: vec![
                Line::from(vec![Span::raw("You: "), Span::raw("hello")]),
                Line::from(vec![Span::raw("     "), Span::raw("world")]),
            ],
            think_hints: Vec::new(),
        };
        let cache = vec![Some(rendered)];
        let sel = Selection::new(Cursor::new(0, 0, 0), Cursor::new(0, 1, 10));
        let text = selected_text(&sel, &messages, &cache);
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
    }
}
