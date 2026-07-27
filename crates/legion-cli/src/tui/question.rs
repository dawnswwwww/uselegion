//! Question prompt formatting.

use crate::tui::state::{PendingQuestion, SUBMIT_LABEL};

/// Format a pending question as plain text for the inline chat message.
///
/// Questions are shown as a horizontal tab bar; the last tab is "Submit".
/// Within a question tab, options are listed vertically (checkbox/radio style)
/// and can be navigated with ↑/↓. The focused tab is bracketed with `>` and
/// `<` so it stands out in plain text.
pub(crate) fn format_question_message(pq: &PendingQuestion) -> String {
    let mut lines = Vec::new();

    // Horizontal tab bar: one tab per question, followed by Submit.
    let mut tabs = Vec::new();
    for (idx, q) in pq.questions.iter().enumerate() {
        let label = if q.header.is_empty() {
            format!("Question {}", idx + 1)
        } else {
            q.header.clone()
        };
        tabs.push(format_tab(&label, idx == pq.current));
    }
    tabs.push(format_tab(SUBMIT_LABEL, pq.is_submit_tab()));
    lines.push(tabs.join("  "));
    lines.push(String::new());

    if pq.is_submit_tab() {
        // Submit tab: show a summary of all selected answers.
        lines.push("Review your answers:".to_string());
        lines.push(String::new());
        for q in &pq.questions {
            let answer_text = pq
                .selected_labels
                .get(&q.question)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    q.options
                        .iter()
                        .filter(|o| s.contains(&o.label))
                        .map(|o| o.label.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "(no selection)".to_string());
            lines.push(format!("[{}] {}: {}", q.header, q.question, answer_text));
        }
        lines.push(String::new());
        lines.push("enter = submit · esc = cancel".to_string());
        return lines.join("\n");
    }

    let q = pq
        .current_question()
        .expect("question tab must have a question");
    lines.push(format!("[{}] {}", q.header, q.question));
    lines.push(String::new());

    for (idx, opt) in q.options.iter().enumerate() {
        let marker = if q.multi_select {
            if pq.is_selected(&q.question, &opt.label) {
                "[x]"
            } else {
                "[ ]"
            }
        } else if pq.is_selected(&q.question, &opt.label) {
            "(*)"
        } else {
            "( )"
        };
        let cursor = if idx == pq.focused { "> " } else { "  " };
        lines.push(format!("{}{} {}", cursor, marker, opt.label));
        if idx == pq.focused {
            lines.push(format!("     {}", opt.description));
        }
    }

    lines.push(String::new());
    let hint = if q.multi_select {
        "↑/↓ select option · space toggle · ←/→ switch tab · enter submit · esc cancel"
    } else {
        "↑/↓ select option · ←/→ switch tab · enter select/submit · esc cancel"
    };
    lines.push(hint.to_string());
    lines.join("\n")
}

/// Render a single tab. The focused tab is wrapped with `>`/`<` so it is
/// visually distinct even in plain text.
pub(crate) fn format_tab(label: &str, focused: bool) -> String {
    if focused {
        format!("> {} <", label)
    } else {
        format!("[ {} ]", label)
    }
}
