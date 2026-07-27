//! Rich multi-line input composer backed by `tui-textarea`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Widget};
use tui_textarea::{CursorMove, Input, Key, TextArea};

/// A thin adapter around [`TextArea`] that provides the input-editing API used
/// by the rest of the TUI.
#[derive(Clone)]
pub struct Composer {
    textarea: TextArea<'static>,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_block(Block::default().borders(Borders::ALL).title("Input"));
        Self { textarea }
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

    /// Mouse support is not wired in this version; the method exists so the
    /// event dispatcher can forward input-area mouse events in the future.
    #[allow(dead_code)]
    pub fn handle_mouse(&mut self, _mouse: crossterm::event::MouseEvent, _area: Rect) {
        // Intentionally no-op: tui-textarea does not expose a mouse API.
    }

    /// Return the current logical lines.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    /// Set the placeholder text shown when the input is empty.
    #[allow(dead_code)]
    pub fn placeholder(&mut self, text: &str) {
        self.textarea.set_placeholder_text(text);
    }

    /// Update the border title (e.g. "shell mode" when the input starts with `!`).
    pub fn set_title(&mut self, title: &'static str) {
        self.textarea
            .set_block(Block::default().borders(Borders::ALL).title(title));
    }

    /// Render the textarea into the given buffer area.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        self.textarea.render(area, buf);
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
}
