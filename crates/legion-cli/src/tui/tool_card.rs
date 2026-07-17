//! Tool-call card rendering.

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
    if total <= limit + 1 {
        for line in text.lines() {
            out.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
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
pub(crate) fn render_tool_card(content: &str, theme: &Theme) -> Vec<Line<'static>> {
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

    let mut lines = vec![Line::from(vec![
        Span::styled("▸ ", Style::default().fg(card_color)),
        Span::styled(
            format!("{} · {}", name, state_label),
            Style::default().fg(card_color).add_modifier(Modifier::BOLD),
        ),
    ])];

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

    lines
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
