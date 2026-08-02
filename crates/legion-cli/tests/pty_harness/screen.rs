//! Minimal VTE screen parser for PTY integration tests.
//!
//! This is intentionally small: it tracks printable characters and the most
//! common cursor-movement / erase sequences emitted by ratatui + crossterm.
//! It does not attempt to faithfully render colors, attributes, or wide cells.

#![allow(dead_code)]

#[derive(Debug, Clone)]
pub struct Screen {
    rows: Vec<Vec<char>>,
    width: usize,
    height: usize,
    cursor_row: usize,
    cursor_col: usize,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

impl Screen {
    pub fn new(height: u16, width: u16) -> Self {
        let height = height as usize;
        let width = width as usize;
        Self {
            rows: vec![vec![' '; width]; height],
            width,
            height,
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    pub fn resize(&mut self, height: u16, width: u16) {
        let height = height as usize;
        let width = width as usize;
        self.rows.resize_with(height, || vec![' '; width]);
        for row in &mut self.rows {
            row.resize(width, ' ');
        }
        self.width = width;
        self.height = height;
        self.cursor_row = self.cursor_row.min(height.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(width.saturating_sub(1));
    }

    pub fn put_char(&mut self, ch: char) {
        if self.cursor_row >= self.height || self.cursor_col >= self.width {
            return;
        }
        // Treat every char as one cell; wide-char accuracy is not required for
        // text-search assertions.
        self.rows[self.cursor_row][self.cursor_col] = ch;
        self.cursor_col += 1;
        if self.cursor_col >= self.width {
            self.cursor_col = 0;
            self.cursor_row += 1;
            if self.cursor_row >= self.height {
                self.scroll_up();
            }
        }
    }

    pub fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    pub fn line_feed(&mut self) {
        self.cursor_row += 1;
        if self.cursor_row >= self.height {
            self.scroll_up();
        }
    }

    pub fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    pub fn tab(&mut self) {
        let next = (self.cursor_col + 8) & !7;
        self.cursor_col = next.min(self.width - 1);
    }

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.height.saturating_sub(1));
        self.cursor_col = col.min(self.width.saturating_sub(1));
    }

    pub fn move_cursor(&mut self, dy: isize, dx: isize) {
        let row = (self.cursor_row as isize + dy).max(0) as usize;
        let col = (self.cursor_col as isize + dx).max(0) as usize;
        self.set_cursor(row, col);
    }

    pub fn clear_screen(&mut self) {
        for row in &mut self.rows {
            row.fill(' ');
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    pub fn erase_down(&mut self) {
        for col in self.cursor_col..self.width {
            self.rows[self.cursor_row][col] = ' ';
        }
        for row in (self.cursor_row + 1)..self.height {
            self.rows[row].fill(' ');
        }
    }

    pub fn erase_line(&mut self, mode: u16) {
        match mode {
            1 => {
                for col in 0..=self.cursor_col {
                    self.rows[self.cursor_row][col] = ' ';
                }
            }
            2 => self.rows[self.cursor_row].fill(' '),
            _ => {
                for col in self.cursor_col..self.width {
                    self.rows[self.cursor_row][col] = ' ';
                }
            }
        }
    }

    pub fn screen_string(&self) -> String {
        self.rows
            .iter()
            .map(|row| {
                let s: String = row.iter().collect();
                s.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn contains(&self, text: &str) -> bool {
        self.screen_string().contains(text)
    }

    fn scroll_up(&mut self) {
        self.rows.remove(0);
        self.rows.push(vec![' '; self.width]);
        self.cursor_row = self.height - 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Normal,
    Escape,
    Csi,
    Osc,
}

/// Streaming parser that feeds bytes into a [`Screen`].
pub struct Parser {
    screen: Screen,
    state: State,
    seq: Vec<u8>,
    utf8_buf: Vec<u8>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            screen: Screen::default(),
            state: State::Normal,
            seq: Vec::new(),
            utf8_buf: Vec::new(),
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.utf8_buf.extend_from_slice(bytes);
        while let Some((ch, len)) = decode_char(&self.utf8_buf) {
            self.handle_char(ch);
            self.utf8_buf.drain(..len);
        }
    }

    fn handle_char(&mut self, ch: char) {
        let byte = ch as u8;
        match self.state {
            State::Normal => {
                if byte == b'\x1b' {
                    self.state = State::Escape;
                    self.seq.clear();
                } else if byte == b'\r' {
                    self.screen.carriage_return();
                } else if byte == b'\n' {
                    self.screen.line_feed();
                } else if byte == 0x08 {
                    self.screen.backspace();
                } else if byte == b'\t' {
                    self.screen.tab();
                } else if byte == b'\x07' {
                    // BEL — ignore.
                } else if byte.is_ascii_control() {
                    // Drop other control bytes.
                } else {
                    self.screen.put_char(ch);
                }
            }
            State::Escape => {
                if byte == b'[' {
                    self.state = State::Csi;
                    self.seq.clear();
                } else if byte == b']' {
                    self.state = State::Osc;
                    self.seq.clear();
                } else {
                    // Single-byte escapes (e.g. ESC-M reverse line feed) are
                    // not handled; return to normal.
                    self.state = State::Normal;
                }
            }
            State::Csi => {
                if (0x40..=0x7E).contains(&byte) {
                    self.handle_csi(byte);
                    self.state = State::Normal;
                    self.seq.clear();
                } else {
                    self.seq.push(byte);
                }
            }
            State::Osc => {
                if byte == b'\x07' {
                    self.state = State::Normal;
                    self.seq.clear();
                } else if byte == b'\x1b' {
                    // Expect ST ('\\') next; swallow it if present.
                    // Simplification: transition to Escape and ignore next char.
                    self.state = State::Escape;
                } else {
                    self.seq.push(byte);
                }
            }
        }
    }

    fn handle_csi(&mut self, command: u8) {
        let seq = std::str::from_utf8(&self.seq).unwrap_or("");
        let params: Vec<u16> = seq.split(';').map(|s| s.parse().unwrap_or(1)).collect();
        match command {
            b'H' | b'f' => {
                let row = params.first().copied().unwrap_or(1).max(1) as usize - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                self.screen.set_cursor(row, col);
            }
            b'A' => {
                let n = params.first().copied().unwrap_or(1) as isize;
                self.screen.move_cursor(-n, 0);
            }
            b'B' => {
                let n = params.first().copied().unwrap_or(1) as isize;
                self.screen.move_cursor(n, 0);
            }
            b'C' => {
                let n = params.first().copied().unwrap_or(1) as isize;
                self.screen.move_cursor(0, n);
            }
            b'D' => {
                let n = params.first().copied().unwrap_or(1) as isize;
                self.screen.move_cursor(0, -n);
            }
            b'G' => {
                let col = params.first().copied().unwrap_or(1).max(1) as usize - 1;
                self.screen.set_cursor(self.screen.cursor_row, col);
            }
            b'J' => match params.first().copied().unwrap_or(0) {
                2 => self.screen.clear_screen(),
                _ => self.screen.erase_down(),
            },
            b'K' => {
                self.screen.erase_line(params.first().copied().unwrap_or(0));
            }
            b'm' | b'h' | b'l' | b's' | b'u' => {
                // SGR, mode set/reset, save/restore cursor — ignored.
            }
            _ => {}
        }
    }
}

fn decode_char(buf: &[u8]) -> Option<(char, usize)> {
    if buf.is_empty() {
        return None;
    }
    let first = buf[0];
    let len = if first < 0x80 {
        1
    } else if first & 0xE0 == 0xC0 {
        2
    } else if first & 0xF0 == 0xE0 {
        3
    } else if first & 0xF8 == 0xF0 {
        4
    } else {
        // Invalid leading byte; drop it.
        return Some(('\u{FFFD}', 1));
    };
    if buf.len() < len {
        return None;
    }
    let s = std::str::from_utf8(&buf[..len]).ok()?;
    s.chars().next().map(|c| (c, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_places_text_at_home() {
        let mut p = Parser::new();
        p.feed(b"Hello\nWorld");
        let text = p.screen().screen_string();
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
    }

    #[test]
    fn parser_handles_clear() {
        let mut p = Parser::new();
        p.feed(b"abc\x1b[2Jxyz");
        let text = p.screen().screen_string();
        assert!(!text.contains("abc"));
        assert!(text.contains("xyz"));
    }

    #[test]
    fn parser_ignores_sgr() {
        let mut p = Parser::new();
        p.feed(b"\x1b[31;1mred\x1b[0m");
        let text = p.screen().screen_string();
        assert!(text.contains("red"));
        assert!(!text.contains('['));
    }
}
