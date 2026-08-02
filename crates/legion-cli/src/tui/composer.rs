//! Rich multi-line input composer backed by `tui-textarea`.
//!
//! Editing state lives in the `TextArea`, but rendering is custom:
//! tui-textarea 0.7 scrolls long lines horizontally instead of wrapping,
//! which conflicts with the layout contract (the input box grows with
//! wrapped content, see `render::plan_layout`). The composer therefore
//! soft-wraps logical lines to the inner width itself and draws the cursor
//! manually.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use tui_textarea::{CursorMove, Input, Key, TextArea};

use crate::tui::input::{char_width, wrap_display_line};

/// A thin adapter around [`TextArea`] that provides the input-editing API used
/// by the rest of the TUI.
#[derive(Clone)]
pub struct Composer {
    textarea: TextArea<'static>,
    title: &'static str,
    border: Color,
    placeholder: String,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    pub fn new() -> Self {
        Self {
            textarea: TextArea::default(),
            title: "Input",
            border: Color::Reset,
            placeholder: String::new(),
        }
    }

    /// Feed a crossterm key event into the textarea.
    pub fn input(&mut self, key: crossterm::event::KeyEvent) {
        self.textarea.input(Input::from(key));
    }

    /// Undo the last edit.
    pub fn undo(&mut self) {
        self.textarea.undo();
    }

    /// Redo the last undo.
    pub fn redo(&mut self) {
        self.textarea.redo();
    }

    /// Move the cursor up one line.
    pub fn move_cursor_up(&mut self) {
        self.textarea.move_cursor(CursorMove::Up);
    }

    /// Move the cursor down one line.
    pub fn move_cursor_down(&mut self) {
        self.textarea.move_cursor(CursorMove::Down);
    }

    /// Move the cursor to the top of the buffer.
    pub fn move_cursor_top(&mut self) {
        self.textarea.move_cursor(CursorMove::Top);
    }

    /// Move the cursor to the bottom of the buffer.
    pub fn move_cursor_bottom(&mut self) {
        self.textarea.move_cursor(CursorMove::Bottom);
    }

    /// Move the cursor to the end of the current line.
    #[allow(dead_code)]
    pub fn move_cursor_end(&mut self) {
        self.textarea.move_cursor(CursorMove::End);
    }

    /// Return the current logical lines.
    pub fn lines(&self) -> Vec<&str> {
        self.textarea.lines().iter().map(String::as_str).collect()
    }

    /// Join the logical lines into a single string.
    pub fn join(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Replace the entire contents with `text` and place the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.textarea.select_all();
        self.textarea.input(delete_input());
        self.textarea.insert_str(text);
        self.textarea.move_cursor(CursorMove::End);
    }

    /// Insert a string at the current cursor position.
    pub fn insert_str(&mut self, text: &str) {
        self.textarea.insert_str(text);
    }

    /// Insert a newline at the cursor (Alt+Enter).
    pub fn insert_newline(&mut self) {
        self.textarea.insert_newline();
    }

    /// Clear the textarea, resetting it to a single empty line.
    pub fn clear(&mut self) {
        self.textarea.select_all();
        self.textarea.input(delete_input());
    }

    /// Return true when the textarea contains no text.
    pub fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    /// Set the placeholder text shown when the input is empty.
    pub fn placeholder(&mut self, text: &str) {
        self.placeholder = text.to_string();
    }

    /// Update the border title and color (e.g. "shell mode" when the input
    /// starts with `!`).
    pub fn set_chrome(&mut self, title: &'static str, border: Color) {
        self.title = title;
        self.border = border;
    }

    /// Render the composer into the given buffer area, soft-wrapping logical
    /// lines to the inner width (tui-textarea itself only scrolls
    /// horizontally). The cursor's logical line is underlined and the cursor
    /// cell is reversed, matching tui-textarea's defaults.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title)
            .border_style(Style::default().fg(self.border));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let width = inner.width as usize;
        let (cur_row, cur_col) = self.textarea.cursor();

        if self.is_empty() {
            if !self.placeholder.is_empty() {
                let line = Line::from(Span::styled(
                    self.placeholder.clone(),
                    Style::default().fg(Color::DarkGray),
                ));
                buf.set_line(inner.x, inner.y, &line, inner.width);
            }
            Self::draw_cursor(buf, inner, inner.x, inner.y);
            return;
        }

        // Wrap each logical line, tracking which visual rows belong to the
        // cursor's logical line (for the underline) and where the cursor
        // lands in visual space.
        let mut visual: Vec<Line<'static>> = Vec::new();
        let mut cursor_visual = 0usize;
        let mut cursor_x = 0usize;
        for (row, logical) in self.textarea.lines().iter().enumerate() {
            let wrapped = wrap_display_line(logical, width);
            if row == cur_row {
                let col_width: usize = logical.chars().take(cur_col).map(char_width).sum();
                cursor_visual = visual.len() + col_width / width;
                cursor_x = col_width % width;
            }
            let style = if row == cur_row {
                Style::default().add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
            };
            visual.extend(wrapped.into_iter().map(|s| Line::from(s).style(style)));
        }

        // Vertical scroll: keep the cursor row visible.
        let height = inner.height as usize;
        let skip = if cursor_visual >= height {
            cursor_visual + 1 - height
        } else {
            0
        };
        for (i, line) in visual.iter().skip(skip).take(height).enumerate() {
            buf.set_line(inner.x, inner.y + i as u16, line, inner.width);
        }

        let cy = inner.y + (cursor_visual - skip) as u16;
        let cx = (inner.x + cursor_x as u16).min(inner.x + inner.width - 1);
        Self::draw_cursor(buf, inner, cx, cy);
    }

    /// Draw the reversed-style cursor cell, clamped to the inner area.
    fn draw_cursor(buf: &mut Buffer, inner: Rect, cx: u16, cy: u16) {
        if cx < inner.x + inner.width && cy < inner.y + inner.height {
            let cell = &mut buf[(cx, cy)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
        }
    }

    /// Return the current cursor position as `(row, col)` in character units.
    #[cfg(test)]
    pub fn cursor(&self) -> (usize, usize) {
        self.textarea.cursor()
    }
}

fn delete_input() -> Input {
    Input {
        key: Key::Delete,
        ctrl: false,
        alt: false,
        shift: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_starts_empty() {
        let composer = Composer::new();
        assert!(composer.is_empty());
        assert_eq!(composer.lines(), vec![""]);
    }

    #[test]
    fn composer_set_text_replaces_content() {
        let mut composer = Composer::new();
        composer.set_text("hello\nworld");
        assert_eq!(composer.lines(), vec!["hello", "world"]);
        assert_eq!(composer.join(), "hello\nworld");
    }

    #[test]
    fn composer_clear_empties_content() {
        let mut composer = Composer::new();
        composer.set_text("hello");
        composer.clear();
        assert!(composer.is_empty());
        assert_eq!(composer.join(), "");
    }

    #[test]
    fn composer_insert_str_appends() {
        let mut composer = Composer::new();
        composer.insert_str("hi");
        composer.insert_str(" there");
        assert_eq!(composer.join(), "hi there");
    }

    fn render_to_buf(composer: &Composer, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, w, h));
        composer.render(ratatui::layout::Rect::new(0, 0, w, h), &mut buf);
        buf
    }

    fn buf_text(buf: &ratatui::buffer::Buffer) -> String {
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn render_soft_wraps_long_lines() {
        let mut composer = Composer::new();
        composer.set_text(&"x".repeat(12));
        // 7 wide => 5 inner columns; 12 chars wrap to 3 visual lines.
        let buf = render_to_buf(&composer, 7, 5);
        let text = buf_text(&buf);
        assert_eq!(text.matches('x').count(), 12, "buffer:\n{}", text);
    }

    #[test]
    fn render_keeps_cursor_visible_by_scrolling() {
        let mut composer = Composer::new();
        // 4 logical lines in a box with 2 content rows: the cursor sits on
        // the last line, so the view must scroll down to it.
        composer.set_text("aaa\nbbb\nccc\nddd");
        let buf = render_to_buf(&composer, 10, 4);
        let text = buf_text(&buf);
        assert!(
            text.contains("ccc") && text.contains("ddd"),
            "buffer:\n{}",
            text
        );
        assert!(
            !text.contains("aaa"),
            "top lines scrolled out; buffer:\n{}",
            text
        );
    }

    #[test]
    fn render_shows_placeholder_when_empty() {
        let mut composer = Composer::new();
        composer.placeholder("type a message");
        let buf = render_to_buf(&composer, 20, 3);
        assert!(buf_text(&buf).contains("type a message"));
    }

    #[test]
    fn render_underlines_cursor_logical_line_only() {
        let mut composer = Composer::new();
        composer.set_text("abc\ndef");
        // Cursor ends on the second logical line after set_text.
        let buf = render_to_buf(&composer, 10, 5);
        let underlined_rows: Vec<u16> = (0..5)
            .filter(|&y| {
                (0..10).any(|x| {
                    buf[(x, y)]
                        .style()
                        .add_modifier
                        .contains(ratatui::style::Modifier::UNDERLINED)
                })
            })
            .collect();
        assert_eq!(
            underlined_rows,
            vec![2],
            "only the cursor line is underlined"
        );
    }
}
