//! Tool-call card rendering.

use crate::tui::ansi::{ansi_to_text, has_ansi};
use crate::tui::links::linkify_text;
use crate::tui::state::{TOOL_ARGS_MAX_CHARS, TOOL_RESULT_HEAD_LINES, TOOL_RESULT_TAIL_LINES};
use crate::tui::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::json;

/// Build the JSON payload stored in a tool message.
pub(crate) fn tool_card_json(
    state: &str,
    name: &str,
    arguments: Option<&str>,
    result: Option<&str>,
) -> String {
    let mut obj = json!({
        "state": state,
        "name": name,
    });
    if let Some(args) = arguments {
        obj["arguments"] = json!(args);
    }
    if let Some(res) = result {
        obj["result"] = json!(res);
    }
    obj.to_string()
}

/// Truncate `s` to at most `max` chars, appending an ellipsis when cut.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Push each line of `text` as `prefix + line` into `out`. Sections longer
/// than head + tail are truncated to the first `TOOL_RESULT_HEAD_LINES` and
/// last `TOOL_RESULT_TAIL_LINES` lines with an omission marker, so a huge
/// tool result cannot flood the scrollback (or the per-frame render cost).
pub(crate) fn push_result_lines(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    style: Style,
    theme: &Theme,
) {
    let total = text.lines().count();
    let limit = TOOL_RESULT_HEAD_LINES + TOOL_RESULT_TAIL_LINES;

    // For ANSI output we render the colored text directly when it fits;
    // truncating styled lines while preserving colors is not worth the
    // complexity for the long-output case, which falls back to plain text.
    if has_ansi(text) && total <= limit + 1 {
        let ansi_text = ansi_to_text(text);
        for line in ansi_text.lines {
            let mut spans = vec![Span::styled(prefix.to_string(), style)];
            spans.extend(line.spans);
            out.push(Line::from(spans));
        }
        return;
    }

    if total <= limit + 1 {
        for line in text.lines() {
            let mut spans = vec![Span::styled(prefix.to_string(), style)];
            spans.extend(linkify_text(line, style));
            out.push(Line::from(spans));
        }
        return;
    }
    for line in text.lines().take(TOOL_RESULT_HEAD_LINES) {
        out.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
    }
    let omitted = total - limit;
    out.push(Line::from(Span::styled(
        format!("{prefix}  … {omitted} lines omitted …"),
        Style::default()
            .fg(theme.tool_bar)
            .add_modifier(Modifier::ITALIC),
    )));
    for line in text.lines().skip(total - TOOL_RESULT_TAIL_LINES) {
        out.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
    }
}

/// Render a tool call as a compact card.
///
/// Collapsed by default: only the title line is shown (`▸ name · state ▶`),
/// keeping agentic turns — which can chain dozens of tool calls — readable.
/// Clicking the card (or `Ctrl+o`) toggles `expanded` to reveal args/result.
/// Error cards collapse too: the red title is the only failure signal, so a
/// failed call's stderr stays tucked until the user opts in. This trades a bit
/// of debug discoverability for a calm scrollback; the toggle is one keystroke.
pub(crate) fn render_tool_card(content: &str, theme: &Theme, expanded: bool) -> ToolCardRender {
    let (state, name, arguments, result) = parse_tool_card(content);

    let card_color = match state.as_str() {
        "done" | "success" => theme.assistant_bar,
        "error" | "failed" => theme.error_fg,
        _ => theme.system_bar,
    };
    let state_label = match state.as_str() {
        "done" | "success" => "done",
        "error" | "failed" => "error",
        "start" => "running",
        _ => &state,
    };

    let hint = if expanded { "▼" } else { "▶" };
    let mut lines = vec![Line::from(vec![
        Span::styled("▸ ", Style::default().fg(card_color)),
        Span::styled(
            format!("{} · {}", name, state_label),
            Style::default().fg(card_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {hint}"), Style::default().fg(theme.tool_bar)),
    ])];

    // Collapsed: nothing beyond the title. args/result stay tucked until the
    // user expands the card.
    if !expanded {
        return ToolCardRender {
            lines,
            header_line: 0,
        };
    }

    if let Some(args) = arguments {
        lines.push(Line::from(Span::styled(
            format!("│ args: {}", truncate_chars(&args, TOOL_ARGS_MAX_CHARS)),
            Style::default().fg(theme.status_fg),
        )));
    }

    if let Some(res) = result {
        // Try to pretty-print structured exec results.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&res) {
            if let Some(exit_code) = value.get("exit_code").and_then(|v| v.as_i64()) {
                let exit_color = if exit_code == 0 {
                    theme.assistant_bar
                } else {
                    theme.error_fg
                };
                lines.push(Line::from(Span::styled(
                    format!("│ exit: {exit_code}"),
                    Style::default().fg(exit_color),
                )));
            }
            if let Some(stdout) = value.get("stdout").and_then(|v| v.as_str()) {
                if !stdout.is_empty() {
                    push_result_lines(
                        &mut lines,
                        "│ → ",
                        stdout,
                        Style::default().fg(theme.status_fg),
                        theme,
                    );
                }
            }
            if let Some(stderr) = value.get("stderr").and_then(|v| v.as_str()) {
                if !stderr.is_empty() {
                    push_result_lines(
                        &mut lines,
                        "│ ✕ ",
                        stderr,
                        Style::default().fg(theme.error_fg),
                        theme,
                    );
                }
            }
            // Fallback for non-exec results.
            if lines.len() == 1 {
                push_result_lines(
                    &mut lines,
                    "│ ",
                    &res,
                    Style::default().fg(theme.status_fg),
                    theme,
                );
            }
        } else {
            push_result_lines(
                &mut lines,
                "│ ",
                &res,
                Style::default().fg(theme.status_fg),
                theme,
            );
        }
    }

    ToolCardRender {
        lines,
        header_line: 0,
    }
}

/// Output of [`render_tool_card`]: the rendered lines plus the line index of
/// the clickable title. Mirrors the `ThinkHint` idea so tool cards can join the
/// same hitbox-driven toggle flow as thinking blocks.
pub(crate) struct ToolCardRender {
    pub(crate) lines: Vec<Line<'static>>,
    /// Index (into `lines`) of the title row that a click should toggle.
    pub(crate) header_line: usize,
}

pub(crate) fn parse_tool_card(content: &str) -> (String, String, Option<String>, Option<String>) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content) {
        let state = value
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("start")
            .to_string();
        let name = value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let arguments = value
            .get("arguments")
            .and_then(|v| v.as_str())
            .map(String::from);
        let result = value
            .get("result")
            .and_then(|v| v.as_str())
            .map(String::from);
        (state, name, arguments, result)
    } else {
        // Legacy fallback for plain-text tool cards.
        let (state, name) = if let Some(rest) = content.strip_prefix("[tool:") {
            if let Some((state, name)) = rest.split_once("] ") {
                (state.to_string(), name.to_string())
            } else {
                ("".to_string(), rest.to_string())
            }
        } else if let Some(rest) = content.strip_prefix("[tool] ") {
            ("".to_string(), rest.to_string())
        } else {
            ("".to_string(), content.to_string())
        };
        (state, name, None, None)
    }
}
