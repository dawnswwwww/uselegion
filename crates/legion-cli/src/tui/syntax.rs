//! Syntax highlighting for code blocks using syntect.

use crate::tui::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::Arc;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

#[derive(Clone)]
pub struct Highlighter {
    ps: SyntaxSet,
    ts: Arc<ThemeSet>,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            ps: SyntaxSet::load_defaults_newlines(),
            ts: Arc::new(ThemeSet::load_defaults()),
        }
    }

    /// Highlight `source` as `lang`. Returns `None` if the language is unknown
    /// or syntect fails to parse the source.
    pub fn highlight_lines(
        &self,
        lang: &str,
        source: &str,
        theme: &Theme,
    ) -> Option<Vec<Line<'static>>> {
        let syntax = self.ps.find_syntax_by_token(lang)?;
        let syntect_theme = self
            .ts
            .themes
            .get(theme.syntect_theme)
            .unwrap_or(&self.ts.themes["base16-ocean.dark"]);
        let mut h = HighlightLines::new(syntax, syntect_theme);
        let mut lines = Vec::new();
        for line in source.lines() {
            let highlighted = h.highlight_line(line, &self.ps).ok()?;
            let spans: Vec<Span<'static>> = highlighted
                .into_iter()
                .map(|(style, text)| syntect_style_to_span(style, text.to_string()))
                .collect();
            lines.push(Line::from(spans));
        }
        Some(lines)
    }
}

fn syntect_style_to_span(style: syntect::highlighting::Style, text: String) -> Span<'static> {
    let mut ratatui_style = Style::default().fg(syntect_color_to_ratatui(style.foreground));
    if style.font_style.contains(FontStyle::BOLD) {
        ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
    }
    Span::styled(text, ratatui_style)
}

fn syntect_color_to_ratatui(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    #[test]
    fn highlight_rust_produces_colored_spans() {
        let h = Highlighter::new();
        let theme = Theme::default();
        let lines = h.highlight_lines("rust", "let x = 1;", &theme).unwrap();
        assert!(!lines.is_empty());
        assert!(!lines[0].spans.is_empty());
    }

    #[test]
    fn unknown_language_returns_none() {
        let h = Highlighter::new();
        let theme = Theme::default();
        assert!(
            h.highlight_lines("not_a_real_lang", "foo", &theme)
                .is_none()
        );
    }
}
