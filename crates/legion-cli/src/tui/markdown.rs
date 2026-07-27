//! Markdown-to-ratatui rendering.

use crate::tui::input::char_width;
use crate::tui::links::osc8_link;
use crate::tui::syntax::Highlighter;
use crate::tui::theme::Theme;
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Render text without markdown parsing. Used while a message is streaming,
/// where partial syntax would flicker and re-parsing every frame is wasted.
pub(crate) fn plain_lines(text: &str) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|l| Line::from(l.to_string()))
        .collect()
}

/// Convert Markdown text into styled `Line`s.
///
/// Supports inline **bold**, *italic*, `code`, ~~strikethrough~~, links, fenced
/// code blocks, headings, unordered/ordered lists, blockquotes and thematic rules.
pub(crate) fn markdown_lines(
    text: &str,
    theme: &Theme,
    highlighter: &Highlighter,
) -> Vec<Line<'static>> {
    let parser = Parser::new(text);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut in_code_block = false;
    let mut code_buffer = String::new();
    let mut code_lang = String::new();
    let mut pending = String::new();
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut quote_depth: usize = 0;
    let mut in_heading: Option<u8> = None;
    let mut link_url: Option<String> = None;
    let mut link_style: Option<Style> = None;

    for event in parser {
        match event {
            MdEvent::Start(tag) => match tag {
                Tag::Strong => style = style.add_modifier(Modifier::BOLD),
                Tag::Emphasis => style = style.add_modifier(Modifier::ITALIC),
                Tag::Strikethrough => style = style.add_modifier(Modifier::CROSSED_OUT),
                Tag::CodeBlock(kind) => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    in_code_block = true;
                    code_lang = match kind {
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                }
                Tag::Heading { level, .. } => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    in_heading = Some(level as u8);
                }
                Tag::List(start) => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    list_stack.push(ListState {
                        ordered: start.is_some(),
                        index: start.unwrap_or(1),
                    });
                }
                Tag::Item => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                }
                Tag::BlockQuote(_) => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    quote_depth += 1;
                }
                Tag::Link { dest_url, .. } => {
                    push_pending_to_spans(
                        &mut current_spans,
                        &mut pending,
                        effective_style(style, in_heading, theme),
                    );
                    link_url = Some(dest_url.to_string());
                    link_style = Some(style);
                    style = style.add_modifier(Modifier::UNDERLINED).fg(theme.link_fg);
                }
                _ => {}
            },
            MdEvent::End(tag_end) => match tag_end {
                TagEnd::Strong => style = style.remove_modifier(Modifier::BOLD),
                TagEnd::Emphasis => style = style.remove_modifier(Modifier::ITALIC),
                TagEnd::Strikethrough => style = style.remove_modifier(Modifier::CROSSED_OUT),
                TagEnd::CodeBlock => {
                    emit_code_block(
                        &mut lines,
                        &mut code_buffer,
                        &mut code_lang,
                        theme,
                        highlighter,
                    );
                    in_code_block = false;
                }
                TagEnd::Heading(..) => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    in_heading = None;
                }
                TagEnd::List(_) => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    list_stack.pop();
                }
                TagEnd::Item => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    if let Some(last) = list_stack.last_mut() {
                        last.index += 1;
                    }
                }
                TagEnd::BlockQuote(_) => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::Link => {
                    if let Some(url) = link_url.take() {
                        let display = std::mem::take(&mut pending);
                        let display_style = effective_style(style, in_heading, theme)
                            .add_modifier(Modifier::UNDERLINED)
                            .fg(theme.link_fg);
                        current_spans.push(Span::styled(osc8_link(&url, &display), display_style));
                    } else {
                        push_pending_to_spans(
                            &mut current_spans,
                            &mut pending,
                            effective_style(style, in_heading, theme),
                        );
                    }
                    if let Some(prev) = link_style.take() {
                        style = prev;
                    }
                }
                _ => {}
            },
            MdEvent::Text(content) => {
                if in_code_block {
                    code_buffer.push_str(&content);
                } else {
                    pending.push_str(&content);
                }
            }
            MdEvent::Code(content) => {
                push_pending_to_spans(
                    &mut current_spans,
                    &mut pending,
                    effective_style(style, in_heading, theme),
                );
                current_spans.push(Span::styled(
                    content.to_string(),
                    Style::default()
                        .fg(theme.inline_code_fg)
                        .bg(theme.code_inline_bg),
                ));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                if in_code_block {
                    code_buffer.push('\n');
                } else {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                }
            }
            MdEvent::Rule => {
                flush_pending(
                    &mut lines,
                    &mut current_spans,
                    &mut pending,
                    style,
                    &active_prefix(&list_stack, quote_depth, in_heading),
                    in_heading,
                    theme,
                );
                lines.push(Line::from(Span::styled(
                    "────────────────────",
                    Style::default().fg(theme.tool_bar),
                )));
            }
            MdEvent::Html(content) | MdEvent::InlineHtml(content) => {
                pending.push_str(&content);
            }
            _ => {}
        }
    }

    if in_code_block {
        emit_code_block(
            &mut lines,
            &mut code_buffer,
            &mut code_lang,
            theme,
            highlighter,
        );
    } else {
        flush_pending(
            &mut lines,
            &mut current_spans,
            &mut pending,
            style,
            &active_prefix(&list_stack, quote_depth, in_heading),
            in_heading,
            theme,
        );
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

#[derive(Clone, Copy)]
pub(crate) struct ListState {
    pub(crate) ordered: bool,
    pub(crate) index: u64,
}

pub(crate) fn active_prefix(
    list_stack: &[ListState],
    quote_depth: usize,
    in_heading: Option<u8>,
) -> String {
    if let Some(level) = in_heading {
        return "# ".repeat(level as usize);
    }
    let mut prefix = String::new();
    for _ in 0..quote_depth {
        prefix.push_str("│ ");
    }
    for list in list_stack {
        if list.ordered {
            prefix.push_str(&format!("{}. ", list.index));
        } else {
            prefix.push_str("• ");
        }
    }
    prefix
}

pub(crate) fn effective_style(style: Style, in_heading: Option<u8>, theme: &Theme) -> Style {
    if let Some(level) = in_heading {
        style
            .fg(theme.heading_color(level))
            .add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

pub(crate) fn push_pending_to_spans(
    spans: &mut Vec<Span<'static>>,
    pending: &mut String,
    style: Style,
) {
    if !pending.is_empty() {
        spans.push(Span::styled(std::mem::take(pending), style));
    }
}

pub(crate) fn flush_pending(
    lines: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    pending: &mut String,
    style: Style,
    prefix: &str,
    in_heading: Option<u8>,
    theme: &Theme,
) {
    let style = effective_style(style, in_heading, theme);
    push_pending_to_spans(spans, pending, style);
    if !spans.is_empty() || !prefix.is_empty() {
        let mut prefixed: Vec<Span> = vec![Span::raw(prefix.to_string())];
        prefixed.append(spans);
        lines.push(Line::from(prefixed));
    }
}

pub(crate) fn emit_code_block(
    lines: &mut Vec<Line<'static>>,
    buffer: &mut String,
    lang: &mut String,
    theme: &Theme,
    highlighter: &Highlighter,
) {
    if buffer.is_empty() {
        return;
    }
    let code_style = Style::default().bg(theme.code_bg).fg(theme.code_fg);
    let gutter_style = Style::default().bg(theme.code_bg).fg(theme.code_gutter_fg);
    let code_lines: Vec<&str> = buffer.trim_end_matches('\n').split('\n').collect();
    let line_num_width = code_lines.len().to_string().len();
    let max_content_width = code_lines
        .iter()
        .map(|l| l.chars().map(char_width).sum::<usize>())
        .max()
        .unwrap_or(0);
    let header_width = (max_content_width + line_num_width + 3).max(24);

    // Top border with language label.
    let label = if lang.is_empty() {
        "code"
    } else {
        lang.as_str()
    };
    let label_width = label.chars().count();
    let border_fill = header_width.saturating_sub(label_width + 4);
    lines.push(Line::from(Span::styled(
        format!("─ {} {}─", label, "─".repeat(border_fill)),
        code_style,
    )));

    let highlighted = highlighter.highlight_lines(lang, buffer, theme);

    for (idx, line) in code_lines.iter().enumerate() {
        let num = idx + 1;
        let mut spans = vec![Span::styled(
            format!("{:>width$} │ ", num, width = line_num_width),
            gutter_style,
        )];
        if !line.is_empty() {
            match highlighted {
                Some(ref hl) if idx < hl.len() => {
                    for span in &hl[idx].spans {
                        let merged = span.style.patch(Style::default().bg(theme.code_bg));
                        spans.push(Span::styled(span.content.clone(), merged));
                    }
                }
                _ => {
                    spans.push(Span::styled(line.to_string(), code_style));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    buffer.clear();
    lang.clear();
}
