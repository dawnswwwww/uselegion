//! OSC 8 hyperlink generation and plain-text URL detection.

use linkify::{LinkFinder, LinkKind};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// Wrap `display` text in an OSC 8 hyperlink pointing to `url`.
///
/// The returned string contains the standard OSC 8 open sequence, the display
/// text, and the closing sequence. Terminals that do not understand OSC 8 will
/// simply render the display text.
pub(crate) fn osc8_link(url: &str, display: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{display}\x1b]8;;\x1b\\")
}

/// Extract URLs from `text`, returning byte-range tuples `(start, end, url)`.
pub(crate) fn extract_urls(text: &str) -> Vec<(usize, usize, String)> {
    let mut finder = LinkFinder::new();
    finder.url_must_have_scheme(true);
    finder
        .links(text)
        .filter(|l| matches!(l.kind(), &LinkKind::Url))
        .map(|l| (l.start(), l.end(), l.as_str().to_string()))
        .collect()
}

/// Split `text` into plain and hyperlink spans.
///
/// URLs are wrapped with OSC 8; non-URL text is left as plain spans. The
/// optional `link_style` is applied to the *display* portion of the hyperlink.
pub(crate) fn linkify_text(text: &str, link_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut last_end = 0;
    for (start, end, url) in extract_urls(text) {
        if start > last_end {
            spans.push(Span::raw(text[last_end..start].to_string()));
        }
        let display = &text[start..end];
        spans.push(Span::styled(
            osc8_link(&url, display),
            link_style.add_modifier(Modifier::UNDERLINED),
        ));
        last_end = end;
    }
    if last_end < text.len() {
        spans.push(Span::raw(text[last_end..].to_string()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc8_link_contains_url_and_display() {
        let s = osc8_link("https://example.com", "example");
        assert!(s.contains("https://example.com"));
        assert!(s.contains("example"));
        // OSC 8 open and close sequences.
        assert!(s.starts_with("\x1b]8;;"));
        assert!(s.ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn extract_urls_finds_http_and_ignores_plain_text() {
        let text = "Visit https://example.com or http://test.org for details.";
        let urls = extract_urls(text);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].2, "https://example.com");
        assert_eq!(urls[1].2, "http://test.org");
    }

    #[test]
    fn linkify_text_splits_plain_and_links() {
        let spans = linkify_text("see https://x.ai here", Style::default());
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "see ");
        assert!(spans[1].content.contains("https://x.ai"));
        assert_eq!(spans[2].content, " here");
    }
}
