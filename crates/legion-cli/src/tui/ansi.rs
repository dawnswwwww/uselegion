//! ANSI escape detection and conversion for command output.

use ratatui::text::Text;

/// Returns true if `text` contains ANSI escape sequences.
pub(crate) fn has_ansi(text: &str) -> bool {
    text.contains('\x1b')
}

/// Convert ANSI text to a ratatui `Text`.
///
/// Falls back to plain text if parsing fails.
pub(crate) fn ansi_to_text(text: &str) -> Text<'static> {
    use ansi_to_tui::IntoText;
    text.into_text()
        .unwrap_or_else(|_| Text::raw(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ansi_escape() {
        assert!(has_ansi("\x1b[31mred\x1b[0m"));
        assert!(!has_ansi("plain text"));
    }

    #[test]
    fn converts_red_text() {
        let text = ansi_to_text("\x1b[31mred\x1b[0m");
        let lines: Vec<_> = text.lines.iter().collect();
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].spans.is_empty());
    }
}
