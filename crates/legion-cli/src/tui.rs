//! Interactive TUI for Legion, similar to Claude Code.

use crate::driver::{
    CliMode, EMBEDDED_NOTICE, LocalDriver, TurnDriver, WsDriver, build_local_host, probe_gateway,
};
use crate::slash_commands::{CommandResult, SlashCommand};
use crate::{CliError, GatewayClient, load_config};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Parser, Tag, TagEnd};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, Wrap,
};
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

/// Pastes longer than this many characters are collapsed into a placeholder.
const PASTE_CHAR_THRESHOLD: usize = 1000;
/// Pastes with more than this many lines are collapsed into a placeholder.
const PASTE_LINE_THRESHOLD: usize = 10;
/// Head lines kept when a tool-card result section (stdout/stderr/text) is truncated.
const TOOL_RESULT_HEAD_LINES: usize = 25;
/// Tail lines kept when a tool-card result section is truncated.
const TOOL_RESULT_TAIL_LINES: usize = 10;
/// Maximum characters of a tool call's arguments shown on the card.
const TOOL_ARGS_MAX_CHARS: usize = 500;
use legion_runtime::{AskUserOutput, AskUserQuestion, TodoItem, TodoStatus};
use legion_skills::SkillRegistry;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthChar;

/// A single message in the TUI chat history.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub state: MessageState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
    /// An interactive `ask_user` prompt rendered inline in the chat history.
    Question,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageState {
    /// Message has been sent but the first token has not arrived yet.
    Loading,
    /// Tokens are streaming in.
    Streaming,
    /// The response finished normally.
    Done,
    /// The response failed or was interrupted.
    Error,
}

impl ChatMessage {
    fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            state: MessageState::Done,
        }
    }
}

/// A command from the TUI loop to the background sender task: either a user
/// message to run as a turn, or a y/n answer to a pending tool-approval
/// prompt (routed to the driver's `resolve_approval`).
#[derive(Debug, Clone, PartialEq)]
enum OutboundControl {
    Message(String),
    /// Run a shell command locally and display the output in the chat.
    ShellCommand(String),
    ResolveApproval {
        prompt_id: String,
        allow: bool,
    },
    /// Answer to an `ask_user` question prompt.
    ResolveQuestion {
        prompt_id: String,
        output: AskUserOutput,
    },
    /// Schedule a `/loop` prompt as a recurring cron job and then run it now.
    ScheduleLoop {
        cron: String,
        prompt: String,
    },
}

#[derive(Default, Clone)]
pub struct AppState {
    messages: Vec<ChatMessage>,
    input: String,
    /// Byte index of the cursor inside `input`.
    cursor: usize,
    /// User inputs sent in this TUI session, recalled with ↑/↓.
    input_history: Vec<String>,
    /// Index into `input_history` when browsing history. `None` means the
    /// current draft is being edited.
    history_index: Option<usize>,
    /// Draft saved when the user first presses ↑, restored by ↓ at the
    /// newest history entry.
    draft_input: Option<String>,
    pub(crate) status: String,
    scroll: usize,
    quit: bool,
    /// Selected index in the slash-command completion menu.
    slash_selected: usize,
    /// Peer id of the current session, shown by `/status` and reused by the
    /// exit-time resume hint.
    pub(crate) session_peer: String,
    /// Which `(message_index, think_index)` blocks are expanded.
    expanded_thinks: HashSet<(usize, usize)>,
    /// Target column for vertical cursor movement across wrapped input lines.
    target_col: Option<usize>,
    /// Cached input area width (inner) for cursor movement calculations.
    input_area_width: u16,
    /// Cached viewport height for scroll clamping.
    viewport_height: u16,
    /// Cached visible chat lines for page scrolling.
    visible_chat_lines: u16,
    /// Cached maximum scroll position (updated each draw).
    max_scroll: usize,
    /// If true, the next draw should snap the message list to the bottom.
    force_scroll_bottom: bool,
    /// True while a user request has been sent but the run has not finished.
    /// Used to keep the status bar in "typing..." state before the first token
    /// arrives, without adding an empty assistant placeholder that would push
    /// the user's own message out of the viewport.
    pending_request: bool,
    /// A tool-approval prompt awaiting the user's y/n answer:
    /// `(prompt_id, tool)`. While set, key input is intercepted by the
    /// approval handler instead of reaching the input box.
    pending_approval: Option<(String, String)>,
    /// An `ask_user` question prompt awaiting the user's answer. While set,
    /// key input is intercepted by the question handler.
    pending_question: Option<PendingQuestion>,
    /// Screen rectangles of thinking hint lines for mouse clicks.
    think_hitboxes: Vec<(Rect, usize, usize)>,
    /// Stored pasted content keyed by placeholder token.
    paste_store: HashMap<String, String>,
    /// Next placeholder id for pasted content.
    next_paste_id: u64,
    /// Per-message render cache, parallel to `messages`. Entries are
    /// re-rendered lazily by `ensure_render_cache` when their inputs change.
    render_cache: Vec<Option<CachedRender>>,
    /// User-invocable skills loaded at TUI startup. Drives the `/skills:`
    /// completion menu and `/<skill-name>` dispatch.
    pub loaded_skills: Vec<legion_skills::Skill>,
    /// Current session todo list, updated by `todo_write` tool events.
    pub todos: Vec<TodoItem>,
    /// When set, the todo panel will be hidden after this instant.
    pub todo_hide_at: Option<std::time::Instant>,
    /// Seconds to keep the todo panel visible after all items are completed.
    pub todo_auto_hide_seconds: u64,
    /// Maximum number of todo items to render in the TUI.
    pub todo_max_display: usize,
    /// Current session goal, if any.
    pub goal: Option<crate::goal::Goal>,
    /// Store used to persist the goal for the current session.
    pub goal_store: crate::goal::GoalStore,
    /// Full session key for the current TUI session.
    pub session_key: String,
}

/// UI state for an in-flight `ask_user` prompt.
#[derive(Clone)]
struct PendingQuestion {
    prompt_id: String,
    questions: Vec<AskUserQuestion>,
    /// Index of the currently visible tab. Tabs are the questions followed by
    /// a final "Submit" tab, so valid indices are `0..=questions.len()`.
    current: usize,
    /// Selected answer labels per question text.
    selected_labels: HashMap<String, HashSet<String>>,
    /// Focused option index within the current *question* tab. Only meaningful
    /// when `current < questions.len()`.
    focused: usize,
    /// Index of the inline message in `AppState.messages` that shows the prompt.
    message_index: usize,
}

/// Label shown on the final confirmation tab.
const SUBMIT_LABEL: &str = "Submit";

impl PendingQuestion {
    fn current_question(&self) -> Option<&AskUserQuestion> {
        self.questions.get(self.current)
    }

    fn is_multi_select(&self) -> bool {
        self.current_question().is_some_and(|q| q.multi_select)
    }

    /// Total number of tabs: one per question plus Submit.
    fn tab_count(&self) -> usize {
        self.questions.len() + 1
    }

    /// Returns `true` when the focus is on the final Submit tab.
    fn is_submit_tab(&self) -> bool {
        self.current == self.questions.len()
    }

    fn is_selected(&self, question: &str, label: &str) -> bool {
        self.selected_labels
            .get(question)
            .is_some_and(|s| s.contains(label))
    }

    fn toggle(&mut self, question: &str, label: &str) {
        let entry = self
            .selected_labels
            .entry(question.to_string())
            .or_default();
        if entry.contains(label) {
            entry.remove(label);
        } else {
            entry.insert(label.to_string());
        }
    }

    fn select_only(&mut self, question: &str, label: &str) {
        self.selected_labels.insert(question.to_string(), {
            let mut s = HashSet::new();
            s.insert(label.to_string());
            s
        });
    }

    fn into_output(self) -> AskUserOutput {
        let mut answers = HashMap::new();
        for q in &self.questions {
            if let Some(set) = self.selected_labels.get(&q.question) {
                if !set.is_empty() {
                    let value = q
                        .options
                        .iter()
                        .filter(|o| set.contains(&o.label))
                        .map(|o| o.label.clone())
                        .collect::<Vec<_>>()
                        .join(",");
                    answers.insert(q.question.clone(), value);
                }
            }
        }
        AskUserOutput {
            questions: self.questions,
            answers,
            annotations: None,
        }
    }
}

/// Format a pending question as plain text for the inline chat message.
///
/// Questions are shown as a horizontal tab bar; the last tab is "Submit".
/// Within a question tab, options are listed vertically (checkbox/radio style)
/// and can be navigated with ↑/↓. The focused tab is bracketed with `>` and
/// `<` so it stands out in plain text.
fn format_question_message(pq: &PendingQuestion) -> String {
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
fn format_tab(label: &str, focused: bool) -> String {
    if focused {
        format!("> {} <", label)
    } else {
        format!("[ {} ]", label)
    }
}

/// Wrap a single `Line` into multiple lines so that each fits within `width`
/// terminal columns. Preserves span styles where possible.
fn wrap_line_to_width(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    if width == 0 {
        return vec![line];
    }

    let str_width = |s: &str| s.chars().map(char_width).sum::<usize>();
    let spans_width =
        |spans: &[Span<'static>]| spans.iter().map(|s| str_width(&s.content)).sum::<usize>();
    if spans_width(&line.spans) <= width {
        return vec![line];
    }

    let mut result = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    let line_style = line.style;
    let spans = line.spans;

    for span in spans {
        let span_width = str_width(&span.content);

        if current_width + span_width <= width {
            current_spans.push(span);
            current_width += span_width;
            continue;
        }

        // Span doesn't fit entirely.
        if current_width > 0 {
            // Flush what we have so far.
            result.push(Line::from(std::mem::take(&mut current_spans)).style(line_style));
            current_width = 0;
        }

        if span_width <= width {
            // The whole span fits on a fresh line.
            current_spans.push(span);
            current_width = span_width;
        } else {
            // Span is wider than the viewport: split it character-by-character.
            let span_style = span.style;
            let mut piece = String::new();
            let mut piece_width = 0usize;
            for c in span.content.chars() {
                let cw = char_width(c);
                if piece_width + cw > width && !piece.is_empty() {
                    result.push(
                        Line::from(vec![Span::styled(std::mem::take(&mut piece), span_style)])
                            .style(line_style),
                    );
                    piece_width = 0;
                }
                piece.push(c);
                piece_width += cw;
            }
            if !piece.is_empty() {
                current_spans.push(Span::styled(piece, span_style));
                current_width = piece_width;
            }
        }
    }

    if !current_spans.is_empty() {
        result.push(Line::from(current_spans).style(line_style));
    }

    if result.is_empty() {
        result.push(Line::from("").style(line_style));
    }

    result
}

impl AppState {
    /// Bring `render_cache` up to date with `messages` for the given viewport
    /// width. Only messages whose rendered inputs changed (content, state,
    /// expanded thinking blocks, width) are re-rendered; everything else is
    /// reused, so a steady-state frame costs ~nothing for old history.
    fn ensure_render_cache(&mut self, width: u16) {
        self.render_cache.truncate(self.messages.len());
        for idx in 0..self.messages.len() {
            let key = render_key(&self.messages[idx], idx, &self.expanded_thinks, width);
            let fresh = self
                .render_cache
                .get(idx)
                .and_then(|e| e.as_ref())
                .is_some_and(|e| e.key == key);
            if fresh {
                continue;
            }
            let role = self.messages[idx].role;
            // Tool cards draw their own borders; everything else gets a 2-col
            // left color bar, so content is wrapped two columns narrower.
            let content_width = if role == MessageRole::Tool {
                width
            } else {
                width.saturating_sub(2)
            };
            let rendered = message_lines(
                &self.messages[idx],
                idx,
                &self.expanded_thinks,
                content_width,
            );
            let (mut lines, think_hints) = wrap_and_remap(rendered, content_width);
            if role != MessageRole::Tool {
                let bar = left_bar_span(role);
                if let Some(bg) = role_background(role) {
                    for line in &mut lines {
                        line.style = line.style.bg(bg);
                        line.spans.insert(0, bar.clone());
                    }
                } else {
                    for line in &mut lines {
                        line.spans.insert(0, bar.clone());
                    }
                }
            }
            if self.render_cache.len() <= idx {
                self.render_cache.resize(idx + 1, None);
            }
            self.render_cache[idx] = Some(CachedRender {
                key,
                lines,
                think_hints,
            });
        }
    }

    /// Total wrapped line count of the full history, including the blank
    /// separator line between messages. Requires `ensure_render_cache` first.
    fn cached_total_lines(&self) -> usize {
        let msg_count = self.messages.len();
        self.render_cache
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                entry.as_ref().map_or(0, |e| e.lines.len()) + usize::from(idx + 1 < msg_count)
            })
            .sum()
    }

    /// Return true if the assistant is currently working on a request.
    /// This covers both the streaming phase and the gap between the user
    /// pressing Enter and the first token arriving.
    fn is_active(&self) -> bool {
        if self.pending_request {
            return true;
        }
        self.messages
            .last()
            .map(|m| {
                m.role == MessageRole::Assistant
                    && (m.state == MessageState::Streaming || m.state == MessageState::Loading)
            })
            .unwrap_or(false)
    }

    /// Number of lines to scroll on PageUp/PageDown.
    fn page_scroll_delta(&self) -> usize {
        let lines = self.visible_chat_lines as usize;
        if lines == 0 { 10 } else { lines }
    }

    /// Completion candidates for the current input, or an empty list when the
    /// input is not a bare command name. Mirrors Claude Code's rule: a
    /// whitespace after the command name (i.e. arguments) closes the menu.
    fn slash_suggestions(&self) -> Vec<SlashCommand> {
        if self.input.starts_with('/') && !self.input.contains(char::is_whitespace) {
            crate::slash_commands::suggestions(&self.input[1..], &self.loaded_skills)
        } else {
            Vec::new()
        }
    }

    /// Append a message to the chat history (used by slash commands).
    pub(crate) fn push_message(&mut self, role: MessageRole, content: impl Into<String>) {
        self.messages.push(ChatMessage::new(role, content));
    }

    /// Clear the local chat view and its render cache. Transcripts on disk
    /// are untouched.
    pub(crate) fn clear_messages(&mut self) {
        self.messages.clear();
        self.render_cache.clear();
    }

    /// Ask the TUI loop to exit (used by `/quit`).
    pub(crate) fn request_quit(&mut self) {
        self.quit = true;
    }

    /// Read-only view of the chat history (slash-command tests).
    #[cfg(test)]
    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }
}

struct RenderedMessage {
    lines: Vec<Line<'static>>,
    /// Hint line numbers index the *unwrapped* `lines`; `wrap_and_remap`
    /// translates them into wrapped-line space for the cache.
    think_hints: Vec<ThinkHint>,
}

#[derive(Clone)]
struct ThinkHint {
    block_index: usize,
    start_line: usize,
    line_count: usize,
}

/// Fingerprint of everything a message's rendered output depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RenderKey {
    content_hash: u64,
    state: MessageState,
    /// Hash of the sorted expanded thinking-block indices for this message.
    expanded_hash: u64,
    width: u16,
}

/// Per-message cached render output: fully wrapped lines plus thinking
/// hints translated into wrapped-line space, relative to the message start.
#[derive(Clone)]
struct CachedRender {
    key: RenderKey,
    lines: Vec<Line<'static>>,
    think_hints: Vec<ThinkHint>,
}

fn render_key(
    msg: &ChatMessage,
    msg_index: usize,
    expanded: &HashSet<(usize, usize)>,
    width: u16,
) -> RenderKey {
    use std::hash::{Hash, Hasher};
    let mut content_hasher = std::collections::hash_map::DefaultHasher::new();
    msg.content.hash(&mut content_hasher);
    let mut expanded_idxs: Vec<usize> = expanded
        .iter()
        .filter(|(m, _)| *m == msg_index)
        .map(|(_, t)| *t)
        .collect();
    expanded_idxs.sort_unstable();
    let mut expanded_hasher = std::collections::hash_map::DefaultHasher::new();
    expanded_idxs.hash(&mut expanded_hasher);
    RenderKey {
        content_hash: content_hasher.finish(),
        state: msg.state,
        expanded_hash: expanded_hasher.finish(),
        width,
    }
}

/// Wrap rendered lines to the viewport width and translate thinking-hint
/// line numbers (which index the unwrapped lines) into wrapped-line space.
fn wrap_and_remap(rendered: RenderedMessage, width: u16) -> (Vec<Line<'static>>, Vec<ThinkHint>) {
    let mut lines = Vec::new();
    let mut wrapped_start = Vec::with_capacity(rendered.lines.len());
    for line in rendered.lines {
        wrapped_start.push(lines.len());
        lines.extend(wrap_line_to_width(line, width));
    }
    let total = lines.len();
    let think_hints = rendered
        .think_hints
        .iter()
        .map(|hint| {
            let start = wrapped_start[hint.start_line];
            let end_orig = hint.start_line + hint.line_count;
            let end = if end_orig < wrapped_start.len() {
                wrapped_start[end_orig]
            } else {
                total
            };
            ThinkHint {
                block_index: hint.block_index,
                start_line: start,
                line_count: end.saturating_sub(start).max(1),
            }
        })
        .collect();
    (lines, think_hints)
}

/// Render text without markdown parsing. Used while a message is streaming,
/// where partial syntax would flicker and re-parsing every frame is wasted.
fn plain_lines(text: &str) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|l| Line::from(l.to_string()))
        .collect()
}

/// Run the interactive TUI.
///
/// `session` resumes a specific session (peer id or full `agent:...` key,
/// see [`crate::resolve_session_key_arg`]); `None` starts a fresh session
/// with a unique peer id. `mode` selects the transport: the gateway
/// WebSocket or an embedded in-process runtime ([`CliMode`]). `yolo`
/// auto-approves every tool prompt instead of showing the approval modal.
pub async fn run_tui(
    session: Option<String>,
    mode: CliMode,
    yolo: bool,
    workspace_override: Option<PathBuf>,
) -> Result<(), CliError> {
    // Resolve (and validate) the session key first so a bad `--session`
    // value fails before setup prompts or the alternate screen.
    let resuming = session.is_some();
    let session_key = match session {
        Some(value) => crate::resolve_session_key_arg(&value, "tui")?,
        None => {
            // Each TUI invocation gets its own session so history does not leak
            // between separate runs. Multi-turn context within the same TUI
            // session is still preserved because every message uses the same
            // session key.
            tui_session_key(&uuid_v4())
        }
    };

    let home_dir = dirs::home_dir()
        .ok_or_else(|| CliError::Other("could not determine home directory".to_string()))?;

    if crate::setup::is_setup_needed(&home_dir) {
        println!("It looks like Legion has not been configured yet.");
        println!("An API key and gateway token are required before using the TUI.");
        if crate::setup::prompt_yes_no("Run interactive setup now?", true)? {
            crate::setup::run_setup(true, crate::setup::SetupOptions::default(), &home_dir).await?;
        } else {
            return Err(CliError::Other(
                "Setup is required. Run `legion setup` and try again.".to_string(),
            ));
        }
    }

    let config = load_config()?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let goal_store = crate::goal::GoalStore::default();
    let mut state_inner = AppState {
        todo_auto_hide_seconds: config.todos.auto_hide_seconds,
        todo_max_display: config.todos.max_display,
        session_key: session_key.clone(),
        goal_store: goal_store.clone(),
        ..AppState::default()
    };

    // Load the persisted goal for this session, if any.
    match goal_store.load(&session_key).await {
        Ok(goal) => state_inner.goal = goal,
        Err(err) => {
            tracing::warn!(error = %err, "failed to load session goal");
        }
    }

    // Load user-invocable skills at startup so the `/skills:` completion menu
    // and `/<skill-name>` dispatch work in the TUI. Mirrors the runtime's
    // skill-loading (agent_loop.rs): config dirs + workspace `.agents/skills`
    // + `.legion/skills`.
    {
        let skills_config = &config.agents.defaults.skills;
        if skills_config.enabled {
            let mut skill_dirs = skills_config.dirs.clone();
            if let Some(ws) = &workspace_override {
                for dir in [".agents/skills", ".legion/skills"] {
                    let p = ws.join(dir);
                    if p.is_dir() {
                        skill_dirs.push(p);
                    }
                }
            }
            let mut registry = legion_skills::SkillRegistryImpl::new();
            let report = registry.load(&skill_dirs).await;
            if !report.loaded.is_empty() {
                tracing::info!(
                    loaded = report.loaded.len(),
                    failed = report.failed.len(),
                    "TUI skills loaded"
                );
            }
            for (path, err) in &report.failed {
                tracing::warn!(path = %path.display(), error = %err, "failed to load skill");
            }
            state_inner.loaded_skills = registry
                .all()
                .iter()
                .filter(|s| s.frontmatter.user_invocable)
                .cloned()
                .collect();
        }
    }

    let state = Arc::new(Mutex::new(state_inner));

    // Select the transport. The WebSocket path behaves exactly as before
    // (Gateway mode still auto-starts the gateway); Auto probes briefly and
    // falls back to an embedded runtime without starting anything.
    let mut version_warning: Option<String> = None;
    let driver: Arc<dyn TurnDriver> = match mode {
        CliMode::Local => Arc::new(LocalDriver::new(
            Arc::new(build_local_host(&config).await?),
            session_key.clone(),
            event_tx.clone(),
            yolo,
            workspace_override.clone(),
        )),
        CliMode::Gateway => {
            // Ensure the gateway is reachable. If not, start it in the background.
            let client = match GatewayClient::connect(&config).await {
                Ok(client) => Arc::new(client),
                Err(_) => {
                    // Try to start the gateway in the background.
                    crate::start_gateway(None, false).await?;
                    // Wait a moment for the gateway to bind.
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    Arc::new(GatewayClient::connect(&config).await?)
                }
            };
            version_warning = client.version_warning().map(str::to_string);
            Arc::new(WsDriver::new(
                client,
                session_key.clone(),
                yolo,
                workspace_override.clone(),
            ))
        }
        CliMode::Auto => match probe_gateway(&config).await {
            Some(client) => {
                version_warning = client.version_warning().map(str::to_string);
                Arc::new(WsDriver::new(
                    Arc::new(client),
                    session_key.clone(),
                    yolo,
                    workspace_override.clone(),
                ))
            }
            None => {
                eprintln!("{EMBEDDED_NOTICE}");
                Arc::new(LocalDriver::new(
                    Arc::new(build_local_host(&config).await?),
                    session_key.clone(),
                    event_tx.clone(),
                    yolo,
                    workspace_override.clone(),
                ))
            }
        },
    };

    state.lock().unwrap().status = driver.mode_name().to_string();

    // On a fresh session show a short welcome instead of a bare workspace hint.
    if !resuming {
        let mode_name = driver.mode_name();
        let ws = workspace_override
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "config default".to_string());
        state.lock().unwrap().messages.push(ChatMessage::new(
            MessageRole::System,
            format!(
                "Welcome to Legion\n\
                 mode: {mode_name} · workspace: {ws}\n\
                 · Enter to send · ↑/↓ history · Shift+↑/↓ cursor · wheel/PgUp/PgDn scroll · Shift+drag select · /help · \
                 !command for local shell · Ctrl+Q to quit"
            ),
        ));
    } else {
        // Surface the active workspace so the user knows which directory the
        // agent works in. Both embedded and gateway modes honor the cwd override.
        let ws = workspace_override
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "config default".to_string());
        state.lock().unwrap().messages.push(ChatMessage::new(
            MessageRole::System,
            format!("workspace: {ws}"),
        ));
    }
    if let Some(warning) = version_warning {
        state
            .lock()
            .unwrap()
            .messages
            .push(ChatMessage::new(MessageRole::System, warning));
    }
    if yolo {
        state.lock().unwrap().messages.push(ChatMessage::new(
            MessageRole::System,
            "yolo mode: tool approvals are auto-accepted".to_string(),
        ));
    }

    // Fetch the resumable history so a resumed session renders its prior
    // turns. This must happen before the driver's reader task is started,
    // otherwise that task would consume the response frame. Only attempted
    // when the user asked to resume — a fresh session's history is empty by
    // construction. Failures are surfaced as a system message instead of
    // being swallowed silently (a stale gateway predating this RPC is the
    // common cause).
    if resuming {
        match tokio::time::timeout(Duration::from_secs(5), driver.history(&session_key)).await {
            Ok(Ok(resp)) if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) => {
                let resumed = history_messages_from_payload(&resp);
                {
                    let mut s = state.lock().unwrap();
                    for msg in &resumed {
                        if msg.role == MessageRole::User {
                            s.input_history.push(msg.content.clone());
                        }
                    }
                    s.messages.extend(resumed);
                }
            }
            Ok(Ok(resp)) => {
                let err = resp
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                state.lock().unwrap().messages.push(ChatMessage::new(
                    MessageRole::System,
                    format!(
                        "failed to load session history: {err} \
                         (stale gateway? restart with `legion gateway stop && legion gateway start`)"
                    ),
                ));
            }
            Ok(Err(err)) => {
                state.lock().unwrap().messages.push(ChatMessage::new(
                    MessageRole::System,
                    format!("failed to load session history: {err}"),
                ));
            }
            Err(_) => {
                state.lock().unwrap().messages.push(ChatMessage::new(
                    MessageRole::System,
                    "timed out loading session history from the gateway".to_string(),
                ));
            }
        }
    }

    // Start the driver's background plumbing now that history is loaded
    // (the WS reader task would otherwise race the history request on the
    // shared connection and consume its response frame).
    driver.start(state.clone(), event_tx.clone());

    // Dispatch task: text from the input box goes to the active driver, and
    // approval answers go to the driver's resolve_approval. The send-failed
    // system message matches the previous WS-only behavior.
    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<OutboundControl>();
    let sender_state = state.clone();
    let sender_driver = Arc::clone(&driver);
    // The sender task uses this clone to wake the UI loop when it mutates
    // state outside of the event flow (the loop only redraws on events).
    let wake_tx = event_tx.clone();
    // The sender task below moves `session_key`; keep the peer id for the
    // exit-time resume hint. `/status` shows the same value.
    let peer_id = crate::session_peer_id(&session_key).to_string();
    state.lock().unwrap().session_peer = peer_id.clone();
    tokio::spawn(async move {
        while let Some(command) = send_rx.recv().await {
            match command {
                OutboundControl::Message(text) => {
                    // Inject the active goal as a user-role context line, per
                    // the OpenClaw /goal spec. Paused/blocked/terminal goals
                    // are not injected so operator stops remain effective.
                    let text = {
                        let s = sender_state.lock().unwrap();
                        if let Some(goal) = &s.goal {
                            if goal.status.is_active() {
                                format!("{}\n\n{}", goal.context_line(), text)
                            } else {
                                text
                            }
                        } else {
                            text
                        }
                    };
                    if let Err(err) = sender_driver.run_turn(text).await {
                        let mut s = sender_state.lock().unwrap();
                        s.messages.push(ChatMessage::new(
                            MessageRole::System,
                            format!("failed to send: {err}"),
                        ));
                        s.messages.last_mut().unwrap().state = MessageState::Error;
                        s.pending_request = false;
                        drop(s);
                        let _ = wake_tx.send(json!({ "type": "internal", "event": "send-failed" }));
                    }
                }
                OutboundControl::ShellCommand(command) => {
                    let output = crate::shell_commands::run_shell_command(&command).await;
                    let mut s = sender_state.lock().unwrap();
                    s.messages
                        .push(ChatMessage::new(MessageRole::System, output));
                    drop(s);
                    let _ = wake_tx.send(json!({ "type": "internal", "event": "shell-done" }));
                }
                OutboundControl::ResolveApproval { prompt_id, allow } => {
                    sender_driver.resolve_approval(&prompt_id, allow).await;
                }
                OutboundControl::ResolveQuestion { prompt_id, output } => {
                    sender_driver.resolve_question(&prompt_id, output).await;
                }
                OutboundControl::ScheduleLoop { cron, prompt } => {
                    match sender_driver.schedule_loop(&cron, &prompt).await {
                        Ok(job_id) => {
                            {
                                let mut s = sender_state.lock().unwrap();
                                s.messages.push(ChatMessage::new(
                                    MessageRole::System,
                                    format!("Loop scheduled as cron job {job_id}. Running the prompt now."),
                                ));
                            }
                            let _ = wake_tx
                                .send(json!({ "type": "internal", "event": "loop-scheduled" }));
                            // Execute the prompt immediately, just like the first cron fire.
                            if let Err(err) = sender_driver.run_turn(prompt).await {
                                {
                                    let mut s = sender_state.lock().unwrap();
                                    s.messages.push(ChatMessage::new(
                                        MessageRole::System,
                                        format!("failed to run loop prompt: {err}"),
                                    ));
                                    s.messages.last_mut().unwrap().state = MessageState::Error;
                                    s.pending_request = false;
                                }
                                let _ = wake_tx
                                    .send(json!({ "type": "internal", "event": "send-failed" }));
                            }
                        }
                        Err(err) => {
                            {
                                let mut s = sender_state.lock().unwrap();
                                s.messages.push(ChatMessage::new(
                                    MessageRole::System,
                                    format!("failed to schedule loop: {err}"),
                                ));
                                s.pending_request = false;
                            }
                            let _ = wake_tx.send(
                                json!({ "type": "internal", "event": "loop-schedule-failed" }),
                            );
                        }
                    }
                }
            }
        }
    });

    let result = run_terminal(state.clone(), send_tx, &mut event_rx).await;

    // Print the resume hint after the alternate screen has been restored so
    // it lands right above the shell prompt — TUI peer ids are generated per
    // launch and are otherwise hard to discover without digging through the
    // transcripts directory.
    println!("session: {peer_id} (resume with `legion --session {peer_id}`)");

    result
}

async fn run_terminal(
    state: Arc<Mutex<AppState>>,
    send_tx: mpsc::UnboundedSender<OutboundControl>,
    event_rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
) -> Result<(), CliError> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Enable mouse capture so the scroll wheel works. Terminal emulators
    // follow a universal convention: holding Shift bypasses the app's mouse
    // capture and falls back to native text selection (click-drag to select,
    // copy). This is how tmux, htop, less, and all ratatui/crossterm apps
    // reconcile "scroll wheel works" with "user can still select text".
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = tui_loop(&mut terminal, state.clone(), send_tx.clone(), event_rx).await;

    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
        DisableBracketedPaste
    )?;

    result
}

async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<AppState>>,
    send_tx: mpsc::UnboundedSender<OutboundControl>,
    event_rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
) -> Result<(), CliError> {
    let mut last_tick = tokio::time::Instant::now();
    let tick_rate = Duration::from_millis(100);
    // Nothing on screen animates, so the UI only redraws when an event may
    // have changed state. Idle iterations block in `poll` and cost no CPU.
    let mut dirty = true;

    loop {
        // Drain incoming websocket events.
        let mut had_events = false;
        while let Ok(msg) = event_rx.try_recv() {
            handle_ws_event(&mut state.lock().unwrap(), msg);
            had_events = true;
        }

        // Poll terminal events with a timeout.
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if crossterm::event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key_event(&mut state.lock().unwrap(), key, &send_tx);
                }
                Event::Paste(text) => {
                    handle_paste(&mut state.lock().unwrap(), text);
                }
                Event::Mouse(mouse) => {
                    handle_mouse_event(&mut state.lock().unwrap(), mouse);
                }
                _ => {}
            }
            had_events = true;
        }
        dirty |= had_events;

        if last_tick.elapsed() >= tick_rate {
            last_tick = tokio::time::Instant::now();
            // Expire the todo panel hide timer if all items are completed.
            let mut s = state.lock().unwrap();
            if s.todo_hide_at
                .is_some_and(|t| t <= std::time::Instant::now())
            {
                s.todos.clear();
                s.todo_hide_at = None;
                dirty = true;
            }
            drop(s);
        }

        let should_quit = state.lock().unwrap().quit;
        if dirty {
            terminal.draw(|f| draw_ui(f, &mut state.lock().unwrap()))?;
            dirty = false;
        }

        if should_quit {
            break;
        }
    }

    Ok(())
}

fn handle_key_event(
    state: &mut AppState,
    key: event::KeyEvent,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    // Quit shortcuts are always honored first.
    if (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('q'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        state.quit = true;
        return;
    }
    // While an `ask_user` prompt is pending, key input is modal.
    if state.pending_question.is_some() {
        handle_question_key(state, key, send_tx);
        return;
    }
    // While a tool-approval prompt is pending, key input is modal: y/n (or
    // Esc) answer the prompt, quit shortcuts still work, and everything else
    // is swallowed so a stray keystroke cannot be misread as an answer.
    if state.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                resolve_pending_approval(state, true, send_tx);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_pending_approval(state, false, send_tx);
            }
            _ => {}
        }
        return;
    }
    // Computed once per key: the completion menu state for `/` input. It is
    // derived from the input alone, so every handler below sees the same view.
    let sugg = state.slash_suggestions();
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.quit = true;
        }
        // Shortcuts require Ctrl so plain typing always reaches the input
        // box — the input is permanently focused, there is no mode in which
        // a bare 'q'/'t' can be interpreted as a command.
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.quit = true;
        }
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            toggle_nearest_thinking(state);
        }
        KeyCode::Enter => {
            if let Some(cmd) = sugg.get(state.slash_selected).cloned() {
                // The completion menu is open: Enter accepts the selection.
                // No-arg commands execute right away; commands that take
                // arguments only complete (like Tab) and wait for input.
                if cmd.arg_hint.is_empty() {
                    let text = format!("/{}", cmd.name);
                    match crate::slash_commands::dispatch(state, &text) {
                        CommandResult::Handled => {}
                        CommandResult::SendToAgent { message } => {
                            state.pending_request = true;
                            let _ = send_tx.send(OutboundControl::Message(message));
                        }
                        CommandResult::ScheduleLoop { interval, prompt } => {
                            state.pending_request = true;
                            let _ = send_tx.send(OutboundControl::ScheduleLoop {
                                cron: interval,
                                prompt,
                            });
                        }
                        CommandResult::NotACommand => {}
                    }
                    commit_and_clear_input(state, &text);
                } else {
                    complete_slash_command(state, &cmd);
                }
            } else {
                let text = expand_paste_placeholders(&state.input, &state.paste_store);
                let text = text.trim().to_string();
                if !text.is_empty() {
                    // Shell escape mode: `!command` runs locally through the
                    // user's shell and shows the output in the chat.
                    if let Some(shell_cmd) = text.strip_prefix('!').map(str::trim) {
                        if shell_cmd.is_empty() {
                            state.messages.push(ChatMessage::new(
                                MessageRole::System,
                                "shell command is empty".to_string(),
                            ));
                        } else {
                            state
                                .messages
                                .push(ChatMessage::new(MessageRole::User, text.clone()));
                            let _ =
                                send_tx.send(OutboundControl::ShellCommand(shell_cmd.to_string()));
                        }
                        commit_and_clear_input(state, &text);
                    } else if text.starts_with('/') {
                        // Slash commands: builtins run locally; skill commands
                        // (/skills:<name>) inject the body and forward to the
                        // agent. Path-like text (`/tmp/x`) falls through.
                        match crate::slash_commands::dispatch(state, &text) {
                            CommandResult::Handled => {
                                commit_and_clear_input(state, &text);
                            }
                            CommandResult::SendToAgent { message } => {
                                commit_and_clear_input(state, &text);
                                state.pending_request = true;
                                let _ = send_tx.send(OutboundControl::Message(message));
                            }
                            CommandResult::ScheduleLoop { interval, prompt } => {
                                commit_and_clear_input(state, &text);
                                state.pending_request = true;
                                let _ = send_tx.send(OutboundControl::ScheduleLoop {
                                    cron: interval,
                                    prompt,
                                });
                            }
                            CommandResult::NotACommand => {
                                // Fall through: treat as a normal message.
                                state
                                    .messages
                                    .push(ChatMessage::new(MessageRole::User, text.clone()));
                                commit_and_clear_input(state, &text);
                                state.pending_request = true;
                                let _ = send_tx.send(OutboundControl::Message(text));
                            }
                        }
                    } else {
                        state
                            .messages
                            .push(ChatMessage::new(MessageRole::User, text.clone()));
                        // Do not add an empty assistant placeholder here. An
                        // empty placeholder still renders to a couple of lines
                        // (prefix + cursor) and, when the viewport is small,
                        // can push the user's own message off-screen. The
                        // assistant row is created lazily by handle_ws_event
                        // when the first delta arrives.
                        commit_and_clear_input(state, &text);
                        state.pending_request = true;
                        let _ = send_tx.send(OutboundControl::Message(text));
                    }
                }
            }
        }
        KeyCode::Tab => {
            if let Some(cmd) = sugg.get(state.slash_selected).cloned() {
                complete_slash_command(state, &cmd);
            }
        }
        KeyCode::Char(c) => {
            insert_char(state, c);
        }
        KeyCode::Backspace => {
            delete_back(state);
        }
        KeyCode::Delete => {
            delete_forward(state);
        }
        KeyCode::Left => {
            move_cursor_left(state, key.modifiers.contains(KeyModifiers::CONTROL));
        }
        KeyCode::Right => {
            move_cursor_right(state, key.modifiers.contains(KeyModifiers::CONTROL));
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if sugg.is_empty() {
                move_cursor_vertical(state, true);
            }
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if sugg.is_empty() {
                move_cursor_vertical(state, false);
            }
        }
        KeyCode::PageUp => {
            let delta = state.page_scroll_delta();
            state.scroll = state.scroll.saturating_sub(delta);
        }
        KeyCode::PageDown => {
            let delta = state.page_scroll_delta();
            state.scroll = state.scroll.saturating_add(delta);
        }
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = 0;
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = state.max_scroll;
        }
        KeyCode::Home if key.modifiers.contains(KeyModifiers::ALT) => {
            move_cursor_home(state, true);
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::ALT) => {
            move_cursor_end(state, true);
        }
        KeyCode::Up => {
            if sugg.is_empty() {
                navigate_input_history(state, true);
            } else {
                // While the completion menu is open, ↑/↓ navigate it.
                state.slash_selected = if state.slash_selected == 0 {
                    sugg.len() - 1
                } else {
                    state.slash_selected - 1
                };
            }
        }
        KeyCode::Down => {
            if sugg.is_empty() {
                navigate_input_history(state, false);
            } else {
                state.slash_selected = (state.slash_selected + 1) % sugg.len();
            }
        }
        KeyCode::Home => {
            move_cursor_home(state, false);
        }
        KeyCode::End => {
            move_cursor_end(state, false);
        }

        _ => {}
    }
}

/// Answer the pending tool-approval prompt: send the decision to the driver
/// (via the sender task) and leave a short note in the chat history.
fn resolve_pending_approval(
    state: &mut AppState,
    allow: bool,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    if let Some((prompt_id, tool)) = state.pending_approval.take() {
        let _ = send_tx.send(OutboundControl::ResolveApproval { prompt_id, allow });
        let decision = if allow { "approved" } else { "denied" };
        state.messages.push(ChatMessage::new(
            MessageRole::System,
            format!("tool '{tool}' {decision}"),
        ));
    }
}

/// Accept the highlighted slash-command completion: replace the input with
/// `/<name> ` and put the cursor at the end, closing the menu.
fn complete_slash_command(state: &mut AppState, cmd: &SlashCommand) {
    state.input = format!("/{} ", cmd.name);
    state.cursor = state.input.len();
    state.target_col = None;
    state.slash_selected = 0;
}

/// Refresh the inline question message so selection changes are visible.
fn refresh_question_message(state: &mut AppState) {
    if let Some(pq) = state.pending_question.as_ref() {
        if let Some(msg) = state.messages.get_mut(pq.message_index) {
            msg.content = format_question_message(pq);
        }
    }
}

/// Handle keys while an `ask_user` prompt is pending.
///
/// Questions are presented as a horizontal tab bar with a final Submit tab.
/// Left/Right arrows switch tabs (wrapping), Up/Down navigate options within
/// the current question tab, Space toggles multi-select options, Enter selects
/// the focused option or submits on the Submit tab, and Esc cancels the prompt.
fn handle_question_key(
    state: &mut AppState,
    key: event::KeyEvent,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    let Some(pq) = state.pending_question.as_mut() else {
        return;
    };
    let tab_count = pq.tab_count();
    match key.code {
        KeyCode::Left => {
            pq.current = (pq.current + tab_count - 1) % tab_count;
            pq.focused = 0;
        }
        KeyCode::Right => {
            pq.current = (pq.current + 1) % tab_count;
            pq.focused = 0;
        }
        KeyCode::Up => {
            if !pq.is_submit_tab() && pq.focused > 0 {
                pq.focused -= 1;
            }
        }
        KeyCode::Down => {
            let option_count = pq.current_question().map(|q| q.options.len());
            if let Some(count) = option_count {
                if pq.focused + 1 < count {
                    pq.focused += 1;
                }
            }
        }
        KeyCode::Char(' ') => {
            if let Some(q) = pq.current_question() {
                if q.multi_select {
                    let question = q.question.clone();
                    let label = q.options[pq.focused].label.clone();
                    pq.toggle(&question, &label);
                }
            }
        }
        KeyCode::Enter => {
            if pq.is_submit_tab() {
                let pq = state
                    .pending_question
                    .take()
                    .expect("submit tab must exist");
                resolve_pending_question(state, send_tx, pq);
                return;
            }
            let (question, label, multi_select) = pq
                .current_question()
                .map(|q| {
                    (
                        q.question.clone(),
                        q.options[pq.focused].label.clone(),
                        q.multi_select,
                    )
                })
                .expect("question tab must have a question");
            if multi_select {
                pq.toggle(&question, &label);
            } else {
                pq.select_only(&question, &label);
            }
            refresh_question_message(state);
            return;
        }
        KeyCode::Esc => {
            cancel_pending_question(state, send_tx);
            return;
        }
        _ => {}
    }
    refresh_question_message(state);
}

/// Cancel the question prompt and answer with an empty selection so the run
/// does not hang forever.
fn cancel_pending_question(state: &mut AppState, send_tx: &mpsc::UnboundedSender<OutboundControl>) {
    let Some(pq) = state.pending_question.take() else {
        return;
    };
    if let Some(msg) = state.messages.get_mut(pq.message_index) {
        msg.content = format!(
            "{}\n\n[cancelled]",
            msg.content.lines().next().unwrap_or("Question")
        );
    }
    let output = AskUserOutput {
        questions: pq.questions.clone(),
        answers: HashMap::new(),
        annotations: None,
    };
    let _ = send_tx.send(OutboundControl::ResolveQuestion {
        prompt_id: pq.prompt_id,
        output,
    });
    state.messages.push(ChatMessage::new(
        MessageRole::System,
        "question cancelled".to_string(),
    ));
}

/// Send the collected answers for a completed question prompt.
fn resolve_pending_question(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
    pq: PendingQuestion,
) {
    let prompt_id = pq.prompt_id.clone();
    let message_index = pq.message_index;
    let output = pq.into_output();
    if let Some(msg) = state.messages.get_mut(message_index) {
        let mut lines = Vec::new();
        for q in &output.questions {
            lines.push(format!("[{}] {}", q.header, q.question));
            if let Some(answer) = output.answers.get(&q.question) {
                for label in answer.split(',') {
                    if let Some(opt) = q.options.iter().find(|o| o.label == label) {
                        lines.push(format!("  ✓ {} — {}", opt.label, opt.description));
                    }
                }
            }
        }
        lines.push(String::new());
        lines.push("[answered]".to_string());
        msg.content = lines.join("\n");
    }
    let summary: Vec<String> = output
        .answers
        .iter()
        .map(|(q, a)| format!("{q}: {a}"))
        .collect();
    let _ = send_tx.send(OutboundControl::ResolveQuestion { prompt_id, output });
    if summary.is_empty() {
        state.messages.push(ChatMessage::new(
            MessageRole::System,
            "question answered (no selection)".to_string(),
        ));
    } else {
        state.messages.push(ChatMessage::new(
            MessageRole::System,
            format!("answered: {}", summary.join("; ")),
        ));
    }
}

fn handle_mouse_event(state: &mut AppState, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.scroll = state.scroll.saturating_sub(3);
        }
        MouseEventKind::ScrollDown => {
            state.scroll = state.scroll.saturating_add(3);
        }
        MouseEventKind::Down(_) => {
            let pos = Position::new(mouse.column, mouse.row);
            for (rect, msg_idx, think_idx) in &state.think_hitboxes {
                if rect.contains(pos) {
                    let key = (*msg_idx, *think_idx);
                    if state.expanded_thinks.contains(&key) {
                        state.expanded_thinks.remove(&key);
                    } else {
                        state.expanded_thinks.insert(key);
                    }
                    break;
                }
            }
        }
        _ => {}
    }
}

fn handle_ws_event(state: &mut AppState, msg: serde_json::Value) {
    match msg.get("type").and_then(|v| v.as_str()) {
        Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some("question") => {
            let payload = msg.get("payload").cloned().unwrap_or(json!({}));
            let prompt_id = payload
                .get("promptId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let questions: Vec<AskUserQuestion> = payload
                .get("questions")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            if !questions.is_empty() {
                let message_index = state.messages.len();
                let pq = PendingQuestion {
                    prompt_id,
                    questions,
                    current: 0,
                    selected_labels: HashMap::new(),
                    focused: 0,
                    message_index,
                };
                let content = format_question_message(&pq);
                state
                    .messages
                    .push(ChatMessage::new(MessageRole::Question, content));
                state.pending_question = Some(pq);
            }
        }
        Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some("approval") => {
            // A Prompt/Required tool is waiting on the user; the status bar
            // renders the prompt and key input turns modal until answered.
            let payload = msg.get("payload").cloned().unwrap_or(json!({}));
            let prompt_id = payload
                .get("promptId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool = payload
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            state.pending_approval = Some((prompt_id, tool));
        }
        Some("event") if msg.get("event").and_then(|v| v.as_str()) == Some("agent") => {
            if let Some(payload) = msg.get("payload") {
                match payload.get("stream").and_then(|v| v.as_str()) {
                    Some("todo_update") => {
                        if let Ok(items) = serde_json::from_value::<Vec<TodoItem>>(
                            payload.get("items").cloned().unwrap_or(json!([])),
                        ) {
                            let all_completed = !items.is_empty()
                                && items.iter().all(|t| t.status == TodoStatus::Completed);
                            state.todos = items;
                            state.todo_hide_at =
                                if all_completed && state.todo_auto_hide_seconds > 0 {
                                    Some(
                                        std::time::Instant::now()
                                            + Duration::from_secs(state.todo_auto_hide_seconds),
                                    )
                                } else {
                                    None
                                };
                        }
                    }
                    Some("lifecycle") => match payload.get("phase").and_then(|v| v.as_str()) {
                        Some("start") => {}
                        Some("end") => {
                            state.pending_request = false;
                            // A still-pending prompt at run end is stale (the
                            // gate timed out); drop it so keys work again.
                            state.pending_approval = None;
                            state.pending_question = None;
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant {
                                    last.state = MessageState::Done;
                                }
                            }
                        }
                        Some("error") => {
                            state.pending_request = false;
                            state.pending_approval = None;
                            state.pending_question = None;
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant {
                                    last.state = MessageState::Error;
                                }
                            }
                            // Surface the error text so a run that fails before
                            // producing any visible output does not look like a
                            // silent hang.
                            if let Some(err) = payload.get("error").and_then(|v| v.as_str()) {
                                let mut msg = ChatMessage::new(
                                    MessageRole::System,
                                    format!("run failed: {err}"),
                                );
                                msg.state = MessageState::Error;
                                state.messages.push(msg);
                            }
                        }
                        _ => {}
                    },
                    Some("assistant") => {
                        if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant
                                    && last.state != MessageState::Done
                                    && last.state != MessageState::Error
                                {
                                    if last.state == MessageState::Loading {
                                        last.state = MessageState::Streaming;
                                    }
                                    last.content.push_str(delta);
                                    return;
                                }
                            }
                            // Start a new assistant turn (e.g. after a tool call).
                            state.messages.push(ChatMessage {
                                role: MessageRole::Assistant,
                                content: delta.to_string(),
                                state: MessageState::Streaming,
                            });
                        }
                    }
                    Some("tool") => {
                        let name = payload
                            .get("tool_call")
                            .and_then(|t| t.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let arguments = payload
                            .get("tool_call")
                            .and_then(|t| t.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let tool_state = payload
                            .get("state")
                            .and_then(|v| v.as_str())
                            .unwrap_or("start");

                        if tool_state == "start" {
                            // The assistant turn ended by deciding to use a tool.
                            if let Some(last) = state.messages.last_mut() {
                                if last.role == MessageRole::Assistant
                                    && (last.state == MessageState::Loading
                                        || last.state == MessageState::Streaming)
                                {
                                    last.state = MessageState::Done;
                                }
                            }
                            state.messages.push(ChatMessage {
                                role: MessageRole::Tool,
                                content: tool_card_json(tool_state, name, Some(arguments), None),
                                state: MessageState::Loading,
                            });
                        } else {
                            let result_content = payload
                                .get("result")
                                .and_then(|r| r.get("content"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let is_error = payload
                                .get("result")
                                .and_then(|r| r.get("is_error"))
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let new_state = if is_error {
                                MessageState::Error
                            } else {
                                MessageState::Done
                            };

                            // Update the most recent unfinished tool card.
                            if let Some(tool) = state.messages.iter_mut().rev().find(|m| {
                                m.role == MessageRole::Tool && m.state == MessageState::Loading
                            }) {
                                tool.content = tool_card_json(
                                    if is_error { "error" } else { "done" },
                                    name,
                                    Some(arguments),
                                    Some(result_content),
                                );
                                tool.state = new_state;
                            } else {
                                state.messages.push(ChatMessage {
                                    role: MessageRole::Tool,
                                    content: tool_card_json(
                                        if is_error { "error" } else { "done" },
                                        name,
                                        Some(arguments),
                                        Some(result_content),
                                    ),
                                    state: new_state,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("res") if msg.get("ok").and_then(|v| v.as_bool()) != Some(true) => {
            let err = msg
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("request failed");
            state
                .messages
                .push(ChatMessage::new(MessageRole::System, err.to_string()));
            state.messages.last_mut().unwrap().state = MessageState::Error;
        }
        _ => {}
    }
}

fn draw_ui(f: &mut ratatui::Frame, state: &mut AppState) {
    state.viewport_height = f.area().height;

    // Dynamic input area: grows with content up to a cap, but always leaves
    // room for the chat (min 5) and the status bar.
    let input_width = f.area().width.saturating_sub(2) as usize;
    let input_line_count = input_visual_lines(&state.input, input_width).len().max(1);
    let input_height = (input_line_count as u16 + 2).clamp(3, 10);
    // On very short terminals keep a compact single-line status bar; otherwise
    // split it into a status line plus a shortcuts line so it is readable.
    let status_height = if f.area().height >= 15 { 2 } else { 1 };

    // Todo panel height: capped by max_display and total height so the chat
    // always keeps at least 5 lines. Hide entirely when empty or short terminal.
    let max_todo_lines = state.todo_max_display.min(10);
    let todo_height = if state.todos.is_empty() || f.area().height < 12 {
        0
    } else {
        let desired = (max_todo_lines as u16 + 2).min(f.area().height / 3).max(3);
        let remaining_for_chat = f
            .area()
            .height
            .saturating_sub(desired + input_height + status_height);
        if remaining_for_chat < 5 { 0 } else { desired }
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(todo_height),
            Constraint::Length(input_height),
            Constraint::Length(status_height),
        ])
        .split(f.area());

    let chat_area = chunks[0];
    let todo_area = chunks[1];
    let input_area = chunks[2];
    state.input_area_width = input_area.width.saturating_sub(2);

    // The Paragraph widget renders inside the chat block's borders, so the
    // usable width is two columns narrower. Pass that to the cache so wrapped
    // lines fit exactly inside the inner area.
    let chat_inner_width = chat_area.width.saturating_sub(2);
    state.ensure_render_cache(chat_inner_width);
    let visible_chat_lines = chat_area.height.saturating_sub(2) as usize;
    let total_lines = state.cached_total_lines();
    let max_scroll = total_lines.saturating_sub(visible_chat_lines);
    state.visible_chat_lines = visible_chat_lines as u16;
    apply_scroll(state, max_scroll);

    // Single pass over the cached per-message renders: collect the visible
    // line window and the thinking-hint hitboxes together.
    state.think_hitboxes.clear();
    let inner_chat = chat_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let window_end = state.scroll + visible_chat_lines;
    let msg_count = state.messages.len();
    let mut visible_lines: Vec<Line> = Vec::new();
    let mut offset = 0usize;
    for (msg_idx, entry) in state.render_cache.iter().enumerate() {
        let Some(entry) = entry else { continue };
        let msg_lines = entry.lines.len();
        // One blank separator line between messages.
        let has_sep = msg_idx + 1 < msg_count;
        let msg_end = offset + msg_lines + usize::from(has_sep);

        if msg_end > state.scroll && offset < window_end {
            let local_start = state.scroll.saturating_sub(offset);
            let local_end = msg_lines.min(window_end - offset);
            visible_lines.extend(entry.lines[local_start..local_end].iter().cloned());
            let sep_pos = offset + msg_lines;
            if has_sep && sep_pos >= state.scroll && sep_pos < window_end {
                visible_lines.push(Line::from(""));
            }
            for hint in &entry.think_hints {
                let global_start = offset + hint.start_line;
                let global_end = global_start + hint.line_count;
                if global_start < window_end && global_end > state.scroll {
                    let start_y = inner_chat.y + global_start.saturating_sub(state.scroll) as u16;
                    let height = global_end.saturating_sub(state.scroll.max(global_start)) as u16;
                    let rect = Rect::new(inner_chat.x, start_y, inner_chat.width, height);
                    state.think_hitboxes.push((rect, msg_idx, hint.block_index));
                }
            }
        }

        offset = msg_end;
        if offset >= window_end {
            break;
        }
    }

    let chat = Paragraph::new(Text::from(visible_lines))
        .block(Block::default().title("Legion").borders(Borders::ALL));
    f.render_widget(chat, chat_area);

    let scrollbar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"));
    f.render_stateful_widget(
        scrollbar,
        chat_area.inner(Margin {
            horizontal: 0,
            vertical: 1,
        }),
        &mut ratatui::widgets::ScrollbarState::new(max_scroll).position(state.scroll),
    );

    // Todo panel.
    if todo_height > 0 && !state.todos.is_empty() {
        let todo_lines = render_todo_panel(state, todo_area.width.saturating_sub(2) as usize);
        let todo = Paragraph::new(Text::from(todo_lines))
            .block(Block::default().title("Tasks").borders(Borders::ALL));
        f.render_widget(todo, todo_area);
    }

    // Input box.
    let inner_input = input_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let visible_height = inner_input.height as usize;
    let input_width = state.input_area_width as usize;
    let input_lines = input_visual_lines(&state.input, input_width);
    let (cursor_line, cursor_col) = cursor_visual_position(&state.input, state.cursor, input_width);
    let input_scroll = cursor_line.saturating_sub(visible_height.saturating_sub(1));
    let displayed_input: Vec<Line> = input_lines
        .into_iter()
        .skip(input_scroll)
        .take(visible_height)
        .map(Line::from)
        .collect();
    let input_title = if state.input.starts_with('!') {
        "shell mode"
    } else {
        "Input"
    };
    let input = Paragraph::new(Text::from(displayed_input))
        .block(Block::default().title(input_title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    f.render_widget(input, input_area);

    let cursor_x = inner_input.x + cursor_col as u16;
    let cursor_y = inner_input.y + (cursor_line - input_scroll) as u16;
    f.set_cursor_position(Position::new(cursor_x, cursor_y));

    // Slash-command completion menu: a floating list above the input box,
    // open while the input is a bare `/name` (see AppState::slash_suggestions).
    let suggestions = state.slash_suggestions();
    if !suggestions.is_empty() {
        let height = (suggestions.len() as u16 + 2).min(input_area.y);
        // A terminal too short for a border plus one row just skips the menu.
        if height >= 3 {
            let area = Rect::new(
                input_area.x,
                input_area.y - height,
                input_area.width,
                height,
            );
            let items: Vec<ListItem> = suggestions
                .iter()
                .enumerate()
                .map(|(idx, cmd)| {
                    let aliases = if cmd.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ({})",
                            cmd.aliases
                                .iter()
                                .map(|alias| format!("/{alias}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let item = ListItem::new(Line::from(format!(
                        "/{}{} — {}",
                        cmd.name, aliases, cmd.description
                    )));
                    if idx == state.slash_selected {
                        item.style(Style::default().add_modifier(Modifier::REVERSED))
                    } else {
                        item
                    }
                })
                .collect();
            let list =
                List::new(items).block(Block::default().title("commands").borders(Borders::ALL));
            f.render_widget(Clear, area);
            f.render_widget(list, area);
        }
    }

    // Status bar. A pending tool-approval prompt takes precedence over the
    // usual status so the user always sees what is blocking the run.
    let (status_text, status_color) = if let Some(pq) = &state.pending_question {
        let hint = if pq.is_submit_tab() {
            "enter=submit · esc=cancel"
        } else if pq.is_multi_select() {
            "←/→ tab · ↑/↓ option · space=toggle · enter=select · esc"
        } else {
            "←/→ tab · ↑/↓ option · enter=select · esc"
        };
        let header = pq
            .current_question()
            .map(|q| q.header.as_str())
            .unwrap_or(SUBMIT_LABEL);
        (format!("{} ({})", header, hint), Color::Yellow)
    } else if let Some((_, tool)) = &state.pending_approval {
        (format!("approve tool '{tool}'? y/n"), Color::Yellow)
    } else if state.is_active() {
        ("typing...".to_string(), Color::Yellow)
    } else {
        (
            state.status.clone(),
            if state.status == "connected" {
                Color::Green
            } else {
                Color::Yellow
            },
        )
    };
    let yolo_hint = if state
        .messages
        .iter()
        .any(|m| m.role == MessageRole::System && m.content.contains("yolo mode"))
    {
        " · yolo"
    } else {
        ""
    };
    let peer_hint = if state.session_peer.is_empty() {
        String::new()
    } else {
        format!(" · {}", state.session_peer)
    };
    let goal_hint = state.goal.as_ref().map(|g| {
        if g.status.is_active() {
            let truncated = if g.objective.chars().count() > 40 {
                format!("{}…", g.objective.chars().take(40).collect::<String>())
            } else {
                g.objective.clone()
            };
            format!(" · goal: {truncated}")
        } else {
            format!(
                " · goal: {} ({})",
                g.status,
                g.objective.chars().take(20).collect::<String>()
            )
        }
    });
    let status_line = Line::from(vec![
        Span::raw("status: "),
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(yolo_hint, Style::default().fg(Color::Red)),
        Span::styled(peer_hint, Style::default().fg(Color::DarkGray)),
        Span::styled(
            goal_hint.unwrap_or_default(),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    let shortcuts_line = Line::from(vec![
        Span::styled("^Q ", Style::default().fg(Color::DarkGray)),
        Span::raw("quit "),
        Span::styled("^T ", Style::default().fg(Color::DarkGray)),
        Span::raw("think "),
        Span::styled("^Enter ", Style::default().fg(Color::DarkGray)),
        Span::raw("send "),
        Span::styled("↑/↓ ", Style::default().fg(Color::DarkGray)),
        Span::raw("history "),
        Span::styled("Shift+↑/↓ ", Style::default().fg(Color::DarkGray)),
        Span::raw("cursor "),
        Span::styled("PgUp/PgDn ", Style::default().fg(Color::DarkGray)),
        Span::raw("scroll "),
        Span::styled("/ ", Style::default().fg(Color::DarkGray)),
        Span::raw("commands "),
        Span::styled("Tab ", Style::default().fg(Color::DarkGray)),
        Span::raw("complete"),
    ]);
    let status_lines = if status_height == 2 {
        vec![status_line, shortcuts_line]
    } else {
        vec![status_line]
    };
    let status = Paragraph::new(Text::from(status_lines));
    f.render_widget(status, chunks[3]);
}

/// Render the todo checklist for the Tasks panel.
///
/// Items are prioritized: in-progress first, then pending, then completed.
/// Each line is truncated to fit the panel width. If there are more items
/// than `todo_max_display`, the last line shows a summary count.
fn render_todo_panel(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let mut ordered: Vec<&TodoItem> = state.todos.iter().collect();
    ordered.sort_by_key(|t| match t.status {
        TodoStatus::InProgress => 0,
        TodoStatus::Pending => 1,
        TodoStatus::Completed => 2,
    });

    let max_lines = state.todo_max_display.max(1);
    let visible: Vec<&TodoItem> = ordered.iter().take(max_lines).copied().collect();
    let hidden = ordered.len().saturating_sub(max_lines);

    let mut lines = Vec::with_capacity(visible.len() + usize::from(hidden > 0));
    for item in visible {
        let (icon, icon_color, text_style) = match item.status {
            TodoStatus::Pending => ("□", Color::DarkGray, Style::default().fg(Color::Gray)),
            TodoStatus::InProgress => (
                "■",
                Color::Yellow,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            TodoStatus::Completed => (
                "✓",
                Color::Green,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
        };

        let mut text = item.content.clone();
        if item.status == TodoStatus::InProgress && !item.active_form.is_empty() {
            text.push_str(&format!(" — {}", item.active_form));
        }
        text = truncate_to_width(&text, width.saturating_sub(2));

        lines.push(Line::from(vec![
            Span::styled(format!("{} ", icon), Style::default().fg(icon_color)),
            Span::styled(text, text_style),
        ]));
    }

    if hidden > 0 {
        let suffix = format!("+{} more", hidden);
        lines.push(Line::from(Span::styled(
            truncate_to_width(&suffix, width),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

/// Truncate a string to fit within `width` display columns, adding an ellipsis
/// when truncation occurs.
fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut current_width = 0usize;
    for c in text.chars() {
        let w = char_width(c);
        if current_width + w > width && !result.is_empty() {
            // Back up and append an ellipsis if there is room.
            if width >= 1 {
                while current_width > width.saturating_sub(1) && !result.is_empty() {
                    let removed = result.pop().unwrap_or('\0');
                    current_width = current_width.saturating_sub(char_width(removed));
                }
                result.push('…');
            }
            return result;
        }
        result.push(c);
        current_width += w;
    }
    result
}

/// Apply a new maximum scroll position while preserving the user's intent.
///
/// - If `force_scroll_bottom` is set, snap to the bottom and clear the flag.
/// - If the user was already at the bottom, keep following new content.
/// - Otherwise keep the current scroll position, clamped to the new max.
fn apply_scroll(state: &mut AppState, max_scroll: usize) {
    let was_at_bottom = state.scroll >= state.max_scroll;
    state.max_scroll = max_scroll;
    if state.force_scroll_bottom || was_at_bottom {
        state.scroll = max_scroll;
        state.force_scroll_bottom = false;
    } else {
        state.scroll = state.scroll.min(max_scroll);
    }
}

// ---------------------------------------------------------------------------
// Input editing helpers
// ---------------------------------------------------------------------------

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1)
}

fn input_visual_lines(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for c in text.chars() {
        let w = char_width(c);
        if current_width + w > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(c);
        current_width += w;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn visual_line_starts(text: &str, width: usize) -> Vec<usize> {
    if width == 0 {
        return vec![0];
    }
    let mut starts = vec![0];
    let mut current_width = 0;
    for (idx, c) in text.char_indices() {
        let w = char_width(c);
        if current_width + w > width && current_width > 0 {
            starts.push(idx);
            current_width = w;
        } else {
            current_width += w;
        }
    }
    starts
}

fn cursor_visual_position(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let starts = visual_line_starts(text, width);
    let line = starts.iter().rposition(|&s| s <= cursor).unwrap_or(0);
    let line_start = starts[line];
    let col = text[line_start..cursor].chars().map(char_width).sum();
    (line, col)
}

fn move_cursor_left(state: &mut AppState, word: bool) {
    if word {
        state.cursor = prev_word_boundary(&state.input, state.cursor);
    } else if state.cursor > 0 {
        let prev = state.input[..state.cursor].chars().next_back().unwrap();
        state.cursor -= prev.len_utf8();
    }
    state.target_col = None;
}

fn move_cursor_right(state: &mut AppState, word: bool) {
    if word {
        state.cursor = next_word_boundary(&state.input, state.cursor);
    } else if state.cursor < state.input.len() {
        let next = state.input[state.cursor..].chars().next().unwrap();
        state.cursor += next.len_utf8();
    }
    state.target_col = None;
}

fn move_cursor_vertical(state: &mut AppState, up: bool) {
    let width = state.input_area_width as usize;
    if width == 0 {
        return;
    }
    let (line, col) = cursor_visual_position(&state.input, state.cursor, width);
    let target = state.target_col.unwrap_or(col);
    let starts = visual_line_starts(&state.input, width);
    let new_line = if up { line.saturating_sub(1) } else { line + 1 };
    if new_line >= starts.len() {
        state.cursor = state.input.len();
        state.target_col = Some(target);
        return;
    }
    let new_start = starts[new_line];
    let new_end = starts
        .get(new_line + 1)
        .copied()
        .unwrap_or(state.input.len());
    let mut current_col = 0;
    let mut new_cursor = new_start;
    for (idx, c) in state.input[new_start..new_end].char_indices() {
        let w = char_width(c);
        if current_col + w > target {
            break;
        }
        current_col += w;
        new_cursor = new_start + idx + c.len_utf8();
    }
    state.cursor = new_cursor;
    state.target_col = Some(target);
}

fn move_cursor_home(state: &mut AppState, full: bool) {
    if full {
        state.cursor = 0;
    } else {
        let width = state.input_area_width as usize;
        let starts = visual_line_starts(&state.input, width);
        let line = starts.iter().rposition(|&s| s <= state.cursor).unwrap_or(0);
        state.cursor = starts[line];
    }
    state.target_col = None;
}

fn move_cursor_end(state: &mut AppState, full: bool) {
    if full {
        state.cursor = state.input.len();
    } else {
        let width = state.input_area_width as usize;
        let starts = visual_line_starts(&state.input, width);
        let line = starts.iter().rposition(|&s| s <= state.cursor).unwrap_or(0);
        state.cursor = starts.get(line + 1).copied().unwrap_or(state.input.len());
    }
    state.target_col = None;
}

fn insert_char(state: &mut AppState, c: char) {
    state.input.insert(state.cursor, c);
    state.cursor += c.len_utf8();
    state.target_col = None;
    state.slash_selected = 0;
}

fn insert_str(state: &mut AppState, text: &str) {
    state.input.insert_str(state.cursor, text);
    state.cursor += text.len();
    state.target_col = None;
}

fn handle_paste(state: &mut AppState, text: String) {
    if text.is_empty() {
        return;
    }

    let line_count = text.lines().count();
    if text.len() > PASTE_CHAR_THRESHOLD || line_count > PASTE_LINE_THRESHOLD {
        let id = state.next_paste_id;
        state.next_paste_id += 1;
        let placeholder = format!(
            "[...Pasted text #{}: {} lines, {} chars...]",
            id,
            line_count,
            text.len()
        );
        state.paste_store.insert(placeholder.clone(), text);
        insert_str(state, &placeholder);
    } else {
        insert_str(state, &text);
    }
    state.slash_selected = 0;
}

fn expand_paste_placeholders(input: &str, store: &HashMap<String, String>) -> String {
    let mut result = input.to_string();
    for (placeholder, content) in store {
        result = result.replace(placeholder, content);
    }
    result
}

fn delete_back(state: &mut AppState) {
    if state.cursor > 0 {
        let prev = state.input[..state.cursor].chars().next_back().unwrap();
        state.cursor -= prev.len_utf8();
        state.input.remove(state.cursor);
    }
    state.target_col = None;
    state.slash_selected = 0;
}

fn delete_forward(state: &mut AppState) {
    if state.cursor < state.input.len() {
        state.input.remove(state.cursor);
    }
    state.target_col = None;
    state.slash_selected = 0;
}

/// Record `text` in the session input history and clear the input box,
/// resetting all transient input state (cursor, target column, paste store).
fn commit_and_clear_input(state: &mut AppState, text: &str) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        state.input_history.push(trimmed.to_string());
    }
    state.history_index = None;
    state.draft_input = None;
    state.input.clear();
    state.cursor = 0;
    state.target_col = None;
    state.slash_selected = 0;
    state.force_scroll_bottom = true;
    state.paste_store.clear();
}

/// Recall previous (↑) or next (↓) user input from `input_history`.
/// The current draft is saved on first ↑ and restored by ↓ past the newest
/// history entry. The cursor is placed at the end of the recalled text.
fn navigate_input_history(state: &mut AppState, up: bool) {
    if state.input_history.is_empty() {
        return;
    }
    if up {
        if state.history_index.is_none() {
            state.draft_input = Some(state.input.clone());
            state.history_index = Some(state.input_history.len() - 1);
        } else if let Some(idx) = state.history_index {
            if idx > 0 {
                state.history_index = Some(idx - 1);
            }
        }
    } else if let Some(idx) = state.history_index {
        if idx + 1 < state.input_history.len() {
            state.history_index = Some(idx + 1);
        } else {
            state.input = state.draft_input.take().unwrap_or_default();
            state.cursor = state.input.len();
            state.history_index = None;
            state.target_col = None;
            state.slash_selected = 0;
            return;
        }
    }
    if let Some(idx) = state.history_index {
        state.input = state.input_history[idx].clone();
        state.cursor = state.input.len();
        state.target_col = None;
        state.slash_selected = 0;
    }
}

fn prev_word_boundary(text: &str, cursor: usize) -> usize {
    let mut chars = text[..cursor].char_indices().rev().peekable();
    // Skip whitespace to the left.
    while let Some((_, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    // Skip the word.
    while let Some((_, c)) = chars.peek() {
        if !c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    chars.next().map(|(idx, _)| idx).unwrap_or(0)
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let mut chars = text[cursor..].char_indices().peekable();
    // Skip current word characters.
    while let Some((_, c)) = chars.peek() {
        if !c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    // Skip whitespace to the right.
    while let Some((_, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    chars
        .peek()
        .map(|(idx, _)| cursor + idx)
        .unwrap_or(text.len())
}

// ---------------------------------------------------------------------------
// Message rendering
// ---------------------------------------------------------------------------

/// A segment of rendered message content.
#[derive(Debug, Clone, PartialEq)]
enum MessageSegment<'a> {
    Text(&'a str),
    Think { index: usize, text: &'a str },
}

/// Parse message content into normal text and `<think>` reasoning segments.
///
/// Unmatched opening `<think>` tags cause all following content to be treated as
/// reasoning until a matching `</think>` is seen. Each think block is assigned an
/// increasing index so the UI can expand/collapse individual blocks.
fn parse_message_segments(content: &str) -> Vec<MessageSegment<'_>> {
    let mut segments = Vec::new();
    let mut rest = content;
    let mut in_think = false;
    let mut think_index = 0;

    while !rest.is_empty() {
        let tag = if in_think { "</think>" } else { "<think>" };
        match rest.find(tag) {
            Some(idx) => {
                let before = &rest[..idx];
                if !before.is_empty() {
                    segments.push(if in_think {
                        MessageSegment::Think {
                            index: think_index,
                            text: before,
                        }
                    } else {
                        MessageSegment::Text(before)
                    });
                }
                rest = &rest[idx + tag.len()..];
                if in_think {
                    think_index += 1;
                }
                in_think = !in_think;
            }
            None => {
                segments.push(if in_think {
                    MessageSegment::Think {
                        index: think_index,
                        text: rest,
                    }
                } else {
                    MessageSegment::Text(rest)
                });
                break;
            }
        }
    }
    segments
}

fn role_color(role: MessageRole) -> Color {
    match role {
        MessageRole::User => Color::Cyan,
        MessageRole::Assistant => Color::Green,
        MessageRole::System => Color::Yellow,
        MessageRole::Tool => Color::DarkGray,
        MessageRole::Question => Color::Magenta,
    }
}

/// Background tint applied to each line of a message to visually group it.
fn role_background(role: MessageRole) -> Option<Color> {
    match role {
        MessageRole::User => Some(Color::Rgb(45, 45, 55)),
        MessageRole::Assistant => Some(Color::Rgb(28, 34, 28)),
        MessageRole::System => Some(Color::Rgb(42, 40, 26)),
        MessageRole::Tool => None,
        MessageRole::Question => Some(Color::Rgb(48, 36, 48)),
    }
}

/// Left edge color bar for a message line.
fn left_bar_span(role: MessageRole) -> Span<'static> {
    Span::styled("█ ", Style::default().fg(role_color(role)))
}

fn state_indicator(role: MessageRole, state: MessageState) -> (&'static str, Color) {
    match role {
        MessageRole::User => ("▸", Color::Cyan),
        MessageRole::System => ("!", Color::Yellow),
        MessageRole::Tool => ("◆", Color::DarkGray),
        MessageRole::Question => ("?", Color::Magenta),
        MessageRole::Assistant => match state {
            MessageState::Loading => ("◐", Color::Yellow),
            MessageState::Streaming => ("◐", Color::Green),
            MessageState::Done => ("●", Color::Green),
            MessageState::Error => ("✕", Color::Red),
        },
    }
}

fn role_label(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "You",
        MessageRole::Assistant => "Legion",
        MessageRole::System => "System",
        MessageRole::Tool => "tool",
        MessageRole::Question => "Question",
    }
}

fn prefix_spans(role: MessageRole, state: MessageState) -> Vec<Span<'static>> {
    let (symbol, symbol_color) = state_indicator(role, state);
    vec![
        Span::styled(symbol, Style::default().fg(symbol_color)),
        Span::raw(" "),
        Span::styled(
            format!("{}:", role_label(role)),
            Style::default()
                .fg(role_color(role))
                .add_modifier(Modifier::BOLD),
        ),
    ]
}

fn prepend_prefix(lines: &mut Vec<Line<'static>>, prefix: Vec<Span<'static>>) {
    if let Some(first) = lines.first_mut() {
        for span in prefix.into_iter().rev() {
            first.spans.insert(0, span);
        }
    } else {
        lines.push(Line::from(prefix));
    }
}

fn message_lines(
    msg: &ChatMessage,
    msg_index: usize,
    expanded: &HashSet<(usize, usize)>,
    _viewport_width: u16,
) -> RenderedMessage {
    if msg.role == MessageRole::Tool {
        let mut lines = render_tool_card(&msg.content);
        if msg.state == MessageState::Streaming || msg.state == MessageState::Loading {
            lines.push(Line::from(Span::styled(
                "▌",
                Style::default().fg(Color::Green),
            )));
        }
        return RenderedMessage {
            lines,
            think_hints: Vec::new(),
        };
    }

    if msg.role == MessageRole::Question {
        // Question prompts are pre-formatted plain text; skip markdown parsing
        // so list markers and checkboxes render literally.
        let lines = plain_lines(&msg.content);
        return RenderedMessage {
            lines,
            think_hints: Vec::new(),
        };
    }

    let prefix = prefix_spans(msg.role, msg.state);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut think_hints: Vec<ThinkHint> = Vec::new();
    let mut first = true;

    for segment in parse_message_segments(&msg.content) {
        match segment {
            MessageSegment::Text(text) => {
                // While tokens are still streaming in, render plain text:
                // partial markdown syntax (an unclosed fence, a half-typed
                // `**`) would flicker, and re-parsing the growing message
                // every frame is wasted work. The full markdown render
                // happens once when the message reaches a terminal state.
                let mut md =
                    if msg.state == MessageState::Streaming || msg.state == MessageState::Loading {
                        plain_lines(text)
                    } else {
                        markdown_lines(text)
                    };
                if first {
                    prepend_prefix(&mut md, prefix.clone());
                    first = false;
                }
                lines.extend(md);
            }
            MessageSegment::Think { index, text } => {
                if text.is_empty() {
                    continue;
                }
                let key = (msg_index, index);
                let is_expanded = expanded.contains(&key);
                let hint_line = lines.len();
                let hint_symbol = if is_expanded { "▼" } else { "▶" };
                let think_prefix = Span::styled(
                    format!("[thinking] {}", hint_symbol),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                );
                let mut hint_line_spans = prefix.clone();
                hint_line_spans.push(think_prefix);
                if first {
                    lines.push(Line::from(hint_line_spans));
                    first = false;
                } else {
                    lines.push(Line::from(vec![Span::styled(
                        format!("[thinking] {}", hint_symbol),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )]));
                }

                if is_expanded {
                    let content_lines = text.split('\n').map(|l| {
                        Line::from(Span::styled(
                            l.to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ))
                    });
                    let content_count = text.split('\n').count();
                    lines.extend(content_lines);
                    think_hints.push(ThinkHint {
                        block_index: index,
                        start_line: hint_line,
                        line_count: 1 + content_count,
                    });
                } else {
                    think_hints.push(ThinkHint {
                        block_index: index,
                        start_line: hint_line,
                        line_count: 1,
                    });
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(prefix));
    }

    if msg.state == MessageState::Streaming || msg.state == MessageState::Loading {
        lines.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(Color::Green),
        )));
    }

    RenderedMessage { lines, think_hints }
}

fn toggle_nearest_thinking(state: &mut AppState) {
    // Toggle the first thinking block in the most recent assistant message.
    for (msg_idx, msg) in state.messages.iter().enumerate().rev() {
        if msg.role != MessageRole::Assistant {
            continue;
        }
        let segments = parse_message_segments(&msg.content);
        if let Some(MessageSegment::Think { index, .. }) = segments
            .iter()
            .find(|s| matches!(s, MessageSegment::Think { .. }))
        {
            let key = (msg_idx, *index);
            if state.expanded_thinks.contains(&key) {
                state.expanded_thinks.remove(&key);
            } else {
                state.expanded_thinks.insert(key);
            }
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------

fn heading_color(level: u8) -> Color {
    match level {
        1 => Color::LightGreen,
        2 => Color::Green,
        3 => Color::Cyan,
        4 => Color::Yellow,
        5 => Color::Magenta,
        _ => Color::DarkGray,
    }
}

#[derive(Clone, Copy)]
struct ListState {
    ordered: bool,
    index: u64,
}

fn active_prefix(list_stack: &[ListState], quote_depth: usize, in_heading: Option<u8>) -> String {
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

fn effective_style(style: Style, in_heading: Option<u8>) -> Style {
    if let Some(level) = in_heading {
        style.fg(heading_color(level)).add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn push_pending_to_spans(spans: &mut Vec<Span<'static>>, pending: &mut String, style: Style) {
    if !pending.is_empty() {
        spans.push(Span::styled(std::mem::take(pending), style));
    }
}

fn flush_pending(
    lines: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    pending: &mut String,
    style: Style,
    prefix: &str,
    in_heading: Option<u8>,
) {
    let style = effective_style(style, in_heading);
    push_pending_to_spans(spans, pending, style);
    if !spans.is_empty() || !prefix.is_empty() {
        let mut prefixed: Vec<Span> = vec![Span::raw(prefix.to_string())];
        prefixed.append(spans);
        lines.push(Line::from(prefixed));
    }
}

/// Convert Markdown text into styled `Line`s.
///
/// Supports inline **bold**, *italic*, `code`, ~~strikethrough~~, links, fenced
/// code blocks, headings, unordered/ordered lists, blockquotes and thematic rules.
fn markdown_lines(text: &str) -> Vec<Line<'static>> {
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
                    );
                    quote_depth += 1;
                }
                Tag::Link { dest_url, .. } => {
                    link_url = Some(dest_url.to_string());
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                _ => {}
            },
            MdEvent::End(tag_end) => match tag_end {
                TagEnd::Strong => style = style.remove_modifier(Modifier::BOLD),
                TagEnd::Emphasis => style = style.remove_modifier(Modifier::ITALIC),
                TagEnd::Strikethrough => style = style.remove_modifier(Modifier::CROSSED_OUT),
                TagEnd::CodeBlock => {
                    emit_code_block(&mut lines, &mut code_buffer, &mut code_lang);
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
                    );
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::Link => {
                    if let Some(url) = link_url.take() {
                        pending.push_str(&format!(" ↗({})", url));
                    }
                    style = style.remove_modifier(Modifier::UNDERLINED);
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
                    effective_style(style, in_heading),
                );
                current_spans.push(Span::styled(
                    content.to_string(),
                    Style::default().bg(Color::Rgb(40, 40, 40)).fg(Color::White),
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
                );
                lines.push(Line::from(Span::styled(
                    "────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            MdEvent::Html(content) | MdEvent::InlineHtml(content) => {
                pending.push_str(&content);
            }
            _ => {}
        }
    }

    if in_code_block {
        emit_code_block(&mut lines, &mut code_buffer, &mut code_lang);
    } else {
        flush_pending(
            &mut lines,
            &mut current_spans,
            &mut pending,
            style,
            &active_prefix(&list_stack, quote_depth, in_heading),
            in_heading,
        );
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn emit_code_block(lines: &mut Vec<Line<'static>>, buffer: &mut String, lang: &mut String) {
    if buffer.is_empty() {
        return;
    }
    let code_style = Style::default()
        .bg(Color::Rgb(30, 30, 30))
        .fg(Color::Rgb(220, 220, 220));
    let gutter_style = Style::default()
        .bg(Color::Rgb(30, 30, 30))
        .fg(Color::DarkGray);
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

    for (idx, line) in code_lines.iter().enumerate() {
        let num = idx + 1;
        let mut spans = vec![Span::styled(
            format!("{:>width$} │ ", num, width = line_num_width),
            gutter_style,
        )];
        if !line.is_empty() {
            spans.push(Span::styled(line.to_string(), code_style));
        }
        lines.push(Line::from(spans));
    }
    buffer.clear();
    lang.clear();
}

/// Build the JSON payload stored in a tool message.
fn tool_card_json(
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
fn truncate_chars(s: &str, max: usize) -> String {
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
fn push_result_lines(out: &mut Vec<Line<'static>>, prefix: &str, text: &str, style: Style) {
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
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
    for line in text.lines().skip(total - TOOL_RESULT_TAIL_LINES) {
        out.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
    }
}

/// Render a tool call as a compact card.
fn render_tool_card(content: &str) -> Vec<Line<'static>> {
    let (state, name, arguments, result) = parse_tool_card(content);

    let card_color = match state.as_str() {
        "done" | "success" => Color::Green,
        "error" | "failed" => Color::Red,
        _ => Color::Yellow,
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
            Style::default().fg(Color::Gray),
        )));
    }

    if let Some(res) = result {
        // Try to pretty-print structured exec results.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&res) {
            if let Some(exit_code) = value.get("exit_code").and_then(|v| v.as_i64()) {
                let exit_color = if exit_code == 0 {
                    Color::Green
                } else {
                    Color::Red
                };
                lines.push(Line::from(Span::styled(
                    format!("│ exit: {exit_code}"),
                    Style::default().fg(exit_color),
                )));
            }
            if let Some(stdout) = value.get("stdout").and_then(|v| v.as_str()) {
                if !stdout.is_empty() {
                    push_result_lines(&mut lines, "│ → ", stdout, Style::default().fg(Color::Gray));
                }
            }
            if let Some(stderr) = value.get("stderr").and_then(|v| v.as_str()) {
                if !stderr.is_empty() {
                    push_result_lines(&mut lines, "│ ✕ ", stderr, Style::default().fg(Color::Red));
                }
            }
            // Fallback for non-exec results.
            if lines.len() == 1 {
                push_result_lines(&mut lines, "│ ", &res, Style::default().fg(Color::Gray));
            }
        } else {
            push_result_lines(&mut lines, "│ ", &res, Style::default().fg(Color::Gray));
        }
    }

    lines
}

fn parse_tool_card(content: &str) -> (String, String, Option<String>, Option<String>) {
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

/// Generate a random RFC 4122 version-4 UUID (e.g.
/// `3f6a1c2e-9b4d-4e8f-a012-9c7b5d3e6f10`) for TUI session and request ids.
///
/// Session peer ids land in transcript file names; they are not secrets, but
/// they must be unique per launch so separate TUI runs never share (and keep
/// growing) the same transcript. Uses the crate's existing `rand` dependency
/// rather than pulling in a `uuid` crate.
pub(crate) fn uuid_v4() -> String {
    let mut b = rand::random::<[u8; 16]>();
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let h = |i: usize| format!("{:02x}", b[i]);
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h(0),
        h(1),
        h(2),
        h(3),
        h(4),
        h(5),
        h(6),
        h(7),
        h(8),
        h(9),
        h(10),
        h(11),
        h(12),
        h(13),
        h(14),
        h(15)
    )
}

/// Extract displayable chat messages from a `sessions.history` response.
///
/// Only user/assistant turns with non-empty content are kept: tool calls and
/// results add noise without helping the user re-read the conversation.
fn history_messages_from_payload(resp: &serde_json::Value) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    if let Some(messages) = resp
        .get("payload")
        .and_then(|p| p.get("messages"))
        .and_then(|m| m.as_array())
    {
        for msg in messages {
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                continue;
            }
            let role = match msg.get("role").and_then(|v| v.as_str()) {
                Some("user") => MessageRole::User,
                Some("assistant") => MessageRole::Assistant,
                _ => continue,
            };
            out.push(ChatMessage::new(role, content.to_string()));
        }
    }
    out
}

/// Build the WebSocket session key used by the TUI.
///
/// The peer id is unique per TUI invocation so that separate runs do not reuse
/// each other's transcripts. Within one TUI session every message shares the
/// same key, so multi-turn context is preserved.
fn tui_session_key(session_id: &str) -> String {
    format!("agent:main:dm:tui:default:direct:{session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::AskUserOption;

    #[test]
    fn message_lines_include_prefix() {
        let msg = ChatMessage::new(MessageRole::User, "hello");
        let rendered = message_lines(&msg, 0, &HashSet::new(), 80);
        let text = rendered.lines[0].to_string();
        assert!(text.contains("You:"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn message_lines_adds_status_indicator() {
        let mut msg = ChatMessage::new(MessageRole::Assistant, "hello");
        msg.state = MessageState::Loading;
        let rendered = message_lines(&msg, 0, &HashSet::new(), 80);
        let text = rendered.lines[0].to_string();
        assert!(text.contains("Legion:"));
        assert!(text.contains("◐"));
    }

    #[test]
    fn message_lines_shows_thinking_hint_before_content() {
        let msg = ChatMessage::new(
            MessageRole::Assistant,
            "<think>secret</think>answer".to_string(),
        );
        let rendered = message_lines(&msg, 0, &HashSet::new(), 80);
        let all_text: String = rendered
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all_text.contains("secret"));
        assert!(all_text.contains("[thinking]"));
        assert!(all_text.contains("▶"));
        assert!(all_text.contains("answer"));
        // Hint line should come before answer line.
        let hint_pos = all_text.find("[thinking]").unwrap();
        let answer_pos = all_text.find("answer").unwrap();
        assert!(hint_pos < answer_pos);
    }

    #[test]
    fn message_lines_expands_specific_think_block() {
        let msg = ChatMessage::new(
            MessageRole::Assistant,
            "<think>secret</think>answer".to_string(),
        );
        let mut expanded = HashSet::new();
        expanded.insert((0, 0));
        let rendered = message_lines(&msg, 0, &expanded, 80);
        let all_text: String = rendered
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("secret"));
        assert!(all_text.contains("▼"));
    }

    #[test]
    fn markdown_lines_renders_bold_and_italic() {
        let lines = markdown_lines("**bold** and *italic*");
        let text = lines[0].to_string();
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
    }

    #[test]
    fn markdown_lines_renders_inline_code() {
        let lines = markdown_lines("use `cargo build`");
        let text = lines[0].to_string();
        assert!(text.contains("cargo build"));
    }

    #[test]
    fn markdown_lines_renders_code_block() {
        let lines = markdown_lines("```rust\nlet x = 1;\n```");
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("let x = 1;"));
        assert!(text.contains("rust"));
        assert!(text.contains("1 │"));
    }

    #[test]
    fn markdown_lines_renders_header() {
        let lines = markdown_lines("# Title\n## Subtitle");
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Title"));
        assert!(text.contains("Subtitle"));
    }

    #[test]
    fn markdown_lines_renders_list() {
        let lines = markdown_lines("- a\n- b");
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("• a"));
        assert!(text.contains("• b"));
    }

    #[test]
    fn markdown_lines_renders_blockquote() {
        let lines = markdown_lines("> quoted");
        let text = lines[0].to_string();
        assert!(text.contains("quoted"));
        assert!(text.contains("│"));
    }

    #[test]
    fn markdown_lines_renders_horizontal_rule() {
        let lines = markdown_lines("---");
        let text = lines[0].to_string();
        assert!(text.contains("──"));
    }

    #[test]
    fn render_tool_card_uses_state_color() {
        let done_lines = render_tool_card("[tool:done] read_file");
        let error_lines = render_tool_card("[tool:error] read_file");
        let start_lines = render_tool_card("[tool:start] read_file");
        assert!(done_lines[0].to_string().contains("done"));
        assert!(error_lines[0].to_string().contains("error"));
        assert!(start_lines[0].to_string().contains("running"));
    }

    #[test]
    fn render_cache_invalidates_on_content_and_width() {
        let mut state = AppState::default();
        state
            .messages
            .push(ChatMessage::new(MessageRole::Assistant, "hello **world**"));
        state.ensure_render_cache(80);
        let key1 = state.render_cache[0].as_ref().unwrap().key;
        state.ensure_render_cache(80);
        assert_eq!(state.render_cache[0].as_ref().unwrap().key, key1);

        state.messages[0].content.push_str(" more");
        state.ensure_render_cache(80);
        let key2 = state.render_cache[0].as_ref().unwrap().key;
        assert_ne!(key2, key1);

        state.ensure_render_cache(40);
        assert_ne!(state.render_cache[0].as_ref().unwrap().key, key2);
    }

    #[test]
    fn cached_total_lines_counts_separators() {
        let mut state = AppState::default();
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "one"));
        state
            .messages
            .push(ChatMessage::new(MessageRole::Assistant, "two"));
        state.ensure_render_cache(80);
        let per_msg: usize = state
            .render_cache
            .iter()
            .map(|e| e.as_ref().unwrap().lines.len())
            .sum();
        assert_eq!(state.cached_total_lines(), per_msg + 1);
    }

    #[test]
    fn streaming_message_renders_plain_text_until_done() {
        let msg = ChatMessage {
            role: MessageRole::Assistant,
            content: "**bold**".to_string(),
            state: MessageState::Streaming,
        };
        let rendered = message_lines(&msg, 0, &HashSet::new(), 80);
        let text: String = rendered
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Raw markers are shown while streaming.
        assert!(text.contains("**bold**"));

        let done = ChatMessage {
            state: MessageState::Done,
            ..msg
        };
        let rendered = message_lines(&done, 0, &HashSet::new(), 80);
        let text: String = rendered
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("**"));
        assert!(text.contains("bold"));
    }

    #[test]
    fn tool_card_truncates_long_stdout() {
        let stdout = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = json!({ "exit_code": 0, "stdout": stdout, "stderr": "" }).to_string();
        let content = tool_card_json("done", "exec", None, Some(&result));
        let lines = render_tool_card(&content);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("line0"));
        assert!(text.contains("line99"));
        assert!(!text.contains("line50"));
        assert!(text.contains("65 lines omitted"));
    }

    #[test]
    fn tool_card_truncates_long_args() {
        let args = "x".repeat(1000);
        let content = tool_card_json("start", "write", Some(&args), None);
        let text: String = render_tool_card(&content)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains('…'));
        assert!(text.len() < 800);
    }

    #[test]
    fn wrap_and_remap_shifts_hints_by_wrapped_lines() {
        let rendered = RenderedMessage {
            // First line wraps to 3 lines at width 10; the hint line fits.
            lines: vec![Line::from("a".repeat(25)), Line::from("[think]")],
            think_hints: vec![ThinkHint {
                block_index: 0,
                start_line: 1,
                line_count: 1,
            }],
        };
        let (lines, hints) = wrap_and_remap(rendered, 10);
        assert_eq!(lines.len(), 4);
        assert_eq!(hints[0].start_line, 3);
        assert_eq!(hints[0].line_count, 1);
    }

    #[test]
    fn handle_ws_event_appends_assistant_delta() {
        let mut state = AppState::default();
        state.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            state: MessageState::Loading,
        });

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "assistant", "delta": "hi" }
        });
        handle_ws_event(&mut state, event);
        assert_eq!(state.messages[0].content, "hi");
        assert_eq!(state.messages[0].state, MessageState::Streaming);
    }

    #[test]
    fn handle_ws_event_marks_done() {
        let mut state = AppState::default();
        state.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: "done".to_string(),
            state: MessageState::Streaming,
        });

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "lifecycle", "phase": "end" }
        });
        handle_ws_event(&mut state, event);
        assert_eq!(state.messages[0].state, MessageState::Done);
    }

    #[test]
    fn handle_ws_event_lifecycle_error_surfaces_error_text() {
        let mut state = AppState::default();
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "hello"));
        state.pending_request = true;

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "lifecycle", "phase": "error", "error": "HTTP 422: nope" }
        });
        handle_ws_event(&mut state, event);

        assert!(!state.pending_request);
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[1].role, MessageRole::System);
        assert!(state.messages[1].content.contains("HTTP 422: nope"));
        assert_eq!(state.messages[1].state, MessageState::Error);
    }

    #[test]
    fn handle_ws_event_tool_start_marks_assistant_done_and_adds_tool() {
        let mut state = AppState::default();
        state.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: "I will check".to_string(),
            state: MessageState::Streaming,
        });

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": {
                "stream": "tool",
                "state": "start",
                "tool_call": { "name": "exec", "arguments": r#"{"command":"ls"}"# }
            }
        });
        handle_ws_event(&mut state, event);

        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].state, MessageState::Done);
        assert_eq!(state.messages[1].role, MessageRole::Tool);
        assert_eq!(state.messages[1].state, MessageState::Loading);
        let text: String = render_tool_card(&state.messages[1].content)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("exec"));
        assert!(text.contains("running"));
    }

    #[test]
    fn handle_ws_event_tool_end_updates_card_with_result() {
        let mut state = AppState::default();
        state.messages.push(ChatMessage {
            role: MessageRole::Tool,
            content: tool_card_json("start", "exec", Some(r#"{"command":"ls"}"#), None),
            state: MessageState::Loading,
        });

        let result = json!({
            "content": r#"{"exit_code":0,"stdout":"file.txt\n","stderr":""}"#,
            "is_error": false
        });
        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": {
                "stream": "tool",
                "state": "end",
                "tool_call": { "name": "exec", "arguments": r#"{"command":"ls"}"# },
                "result": result
            }
        });
        handle_ws_event(&mut state, event);

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].state, MessageState::Done);
        let text: String = render_tool_card(&state.messages[0].content)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("done"));
        assert!(text.contains("file.txt"));
    }

    #[test]
    fn handle_ws_event_assistant_delta_after_tool_starts_new_turn() {
        let mut state = AppState::default();
        state.messages.push(ChatMessage {
            role: MessageRole::Tool,
            content: tool_card_json("done", "exec", None, Some("ok")),
            state: MessageState::Done,
        });

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "assistant", "delta": "Here is the result." }
        });
        handle_ws_event(&mut state, event);

        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[1].role, MessageRole::Assistant);
        assert_eq!(state.messages[1].content, "Here is the result.");
        assert_eq!(state.messages[1].state, MessageState::Streaming);
    }

    #[test]
    fn handle_ws_event_approval_sets_pending_approval() {
        let mut state = AppState::default();

        let event = json!({
            "type": "event",
            "event": "approval",
            "payload": {
                "promptId": "prompt-0",
                "tool": "exec",
                "agentId": "main",
                "sessionKey": "agent:main:dm:tui:default:direct:p1"
            }
        });
        handle_ws_event(&mut state, event);

        assert_eq!(
            state.pending_approval,
            Some(("prompt-0".to_string(), "exec".to_string()))
        );
    }

    #[test]
    fn handle_ws_event_lifecycle_end_clears_stale_approval() {
        let mut state = AppState {
            pending_request: true,
            pending_approval: Some(("prompt-0".to_string(), "exec".to_string())),
            ..AppState::default()
        };

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "lifecycle", "phase": "end" }
        });
        handle_ws_event(&mut state, event);

        assert!(
            state.pending_approval.is_none(),
            "a prompt left pending at run end (gate timeout) must be cleared"
        );
    }

    #[test]
    fn handle_key_event_y_approves_pending_tool() {
        let mut state = AppState {
            pending_approval: Some(("prompt-0".to_string(), "exec".to_string())),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('y')), &tx);

        assert_eq!(
            rx.try_recv().expect("approval answer must be sent"),
            OutboundControl::ResolveApproval {
                prompt_id: "prompt-0".to_string(),
                allow: true,
            }
        );
        assert!(state.pending_approval.is_none());
        let note = state.messages.last().expect("a decision note is shown");
        assert_eq!(note.role, MessageRole::System);
        assert!(note.content.contains("approved"));
    }

    #[test]
    fn handle_key_event_n_denies_pending_tool() {
        let mut state = AppState {
            pending_approval: Some(("prompt-1".to_string(), "exec".to_string())),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('n')), &tx);

        assert_eq!(
            rx.try_recv().expect("approval answer must be sent"),
            OutboundControl::ResolveApproval {
                prompt_id: "prompt-1".to_string(),
                allow: false,
            }
        );
        assert!(state.pending_approval.is_none());
        let note = state.messages.last().expect("a decision note is shown");
        assert!(note.content.contains("denied"));
    }

    #[test]
    fn handle_key_event_esc_denies_pending_tool() {
        let mut state = AppState {
            pending_approval: Some(("prompt-2".to_string(), "exec".to_string())),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Esc), &tx);

        assert_eq!(
            rx.try_recv().expect("approval answer must be sent"),
            OutboundControl::ResolveApproval {
                prompt_id: "prompt-2".to_string(),
                allow: false,
            }
        );
    }

    #[test]
    fn handle_key_event_swallows_other_keys_while_approval_pending() {
        let mut state = AppState {
            pending_approval: Some(("prompt-3".to_string(), "exec".to_string())),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();

        // Neither text input nor an accidental Enter may leak through while
        // the approval prompt is modal.
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('a')), &tx);
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        assert!(state.input.is_empty());
        assert!(
            state.pending_approval.is_some(),
            "prompt stays pending until y/n/Esc"
        );
        assert!(rx.try_recv().is_err(), "no command may be sent");
    }

    #[test]
    fn handle_key_event_sends_user_message_and_marks_pending() {
        let mut state = AppState {
            input: "hi".to_string(),
            cursor: 2,
            ..AppState::default()
        };
        let (tx, _rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        // Only the user message is added immediately; the assistant placeholder
        // is created lazily when the first token streams in.
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::User);
        assert_eq!(state.messages[0].content, "hi");
        assert!(state.pending_request);
        assert!(state.force_scroll_bottom);
        assert!(state.input.is_empty());
    }

    #[test]
    fn handle_key_event_slash_help_runs_locally_without_sending() {
        let mut state = AppState {
            input: "/help".to_string(),
            cursor: 5,
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        // A User echo plus the System help listing, nothing else.
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].role, MessageRole::User);
        assert_eq!(state.messages[0].content, "/help");
        assert_eq!(state.messages[1].role, MessageRole::System);
        assert!(state.messages[1].content.contains("/clear"));
        assert!(!state.pending_request, "slash commands never start a turn");
        assert!(state.input.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "slash commands must not reach the driver"
        );
    }

    #[test]
    fn handle_key_event_slash_quit_sets_quit_flag() {
        let mut state = AppState {
            input: "/quit".to_string(),
            cursor: 5,
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert!(state.quit);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handle_key_event_slash_clear_empties_history() {
        let mut state = AppState {
            input: "/clear".to_string(),
            cursor: 6,
            ..AppState::default()
        };
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "old"));
        state.render_cache.push(None);
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert!(
            state.messages.is_empty(),
            "the command echo is cleared along with the history"
        );
        assert!(state.render_cache.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handle_key_event_slash_path_is_sent_as_message() {
        let mut state = AppState {
            input: "/tmp/foo".to_string(),
            cursor: 8,
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert_eq!(
            rx.try_recv()
                .expect("path-like input is sent as a normal message"),
            OutboundControl::Message("/tmp/foo".to_string())
        );
        assert!(state.pending_request);
        assert_eq!(state.messages[0].role, MessageRole::User);
    }

    #[test]
    fn handle_key_event_tab_completes_selected_slash_command() {
        let mut state = AppState {
            input: "/he".to_string(),
            cursor: 3,
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Tab), &tx);
        assert_eq!(state.input, "/help ");
        assert_eq!(state.cursor, 6);
        assert_eq!(state.slash_selected, 0);
        assert!(rx.try_recv().is_err(), "completion does not send anything");
    }

    #[test]
    fn handle_key_event_arrows_navigate_slash_menu_without_moving_cursor() {
        let (tx, _rx) = mpsc::unbounded_channel::<OutboundControl>();
        let mut state = AppState {
            input: "/".to_string(),
            cursor: 1,
            input_area_width: 80,
            ..AppState::default()
        };
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.slash_selected, 1);
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.slash_selected, 2);
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.slash_selected, 1);
        assert_eq!(state.cursor, 1, "menu navigation must not move the cursor");

        // With a single suggestion, a bare Down would otherwise move the
        // cursor to the end of the input line (via move_cursor_vertical).
        let mut state = AppState {
            input: "/help".to_string(),
            cursor: 3,
            input_area_width: 80,
            ..AppState::default()
        };
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.slash_selected, 0);
        assert_eq!(state.cursor, 3);
    }

    #[test]
    fn commit_and_clear_input_records_history() {
        let mut state = AppState {
            input: "hello".to_string(),
            cursor: 5,
            ..AppState::default()
        };
        commit_and_clear_input(&mut state, "hello");
        assert_eq!(state.input_history, vec!["hello".to_string()]);
        assert!(state.input.is_empty());
        assert_eq!(state.cursor, 0);
        assert!(state.draft_input.is_none());
        assert!(state.history_index.is_none());
    }

    #[test]
    fn handle_key_event_up_down_recalls_input_history() {
        let (tx, _rx) = mpsc::unbounded_channel::<OutboundControl>();
        let mut state = AppState {
            input_history: vec!["first".to_string(), "second".to_string()],
            input_area_width: 80,
            ..AppState::default()
        };

        // ↑ recalls the most recent entry.
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.input, "second");
        assert_eq!(state.cursor, 6);
        assert_eq!(state.history_index, Some(1));
        assert_eq!(state.draft_input, Some(String::new()));

        // ↑ again moves to older entry.
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.input, "first");
        assert_eq!(state.history_index, Some(0));

        // ↓ moves forward through history.
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.input, "second");
        assert_eq!(state.history_index, Some(1));
    }

    #[test]
    fn handle_key_event_down_restores_draft() {
        let (tx, _rx) = mpsc::unbounded_channel::<OutboundControl>();
        let mut state = AppState {
            input: "draft".to_string(),
            cursor: 5,
            input_history: vec!["previous".to_string()],
            input_area_width: 80,
            ..AppState::default()
        };

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.input, "previous");
        assert_eq!(state.draft_input, Some("draft".to_string()));

        // ↓ past the newest entry restores the saved draft.
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.input, "draft");
        assert_eq!(state.cursor, 5);
        assert!(state.history_index.is_none());
        assert!(state.draft_input.is_none());
    }

    #[test]
    fn handle_key_event_shift_up_down_moves_cursor_vertically() {
        let (tx, _rx) = mpsc::unbounded_channel::<OutboundControl>();
        let mut state = AppState {
            input: "abcd efgh".to_string(),
            cursor: 0,
            input_area_width: 5,
            ..AppState::default()
        };

        fn shift_key(code: KeyCode) -> event::KeyEvent {
            event::KeyEvent {
                code,
                modifiers: KeyModifiers::SHIFT,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::empty(),
            }
        }

        handle_key_event(&mut state, shift_key(KeyCode::Down), &tx);
        assert_eq!(
            state.cursor, 5,
            "Shift+Down should move to next visual line"
        );
        handle_key_event(&mut state, shift_key(KeyCode::Up), &tx);
        assert_eq!(
            state.cursor, 0,
            "Shift+Up should move to previous visual line"
        );
    }

    #[test]
    fn navigate_input_history_no_ops_when_empty() {
        let mut state = AppState::default();
        navigate_input_history(&mut state, true);
        assert!(state.input.is_empty());
        assert!(state.history_index.is_none());
    }

    #[test]
    fn handle_key_event_slash_command_with_args_executes_directly() {
        let mut state = AppState {
            input: "/help me".to_string(),
            cursor: 8,
            ..AppState::default()
        };
        assert!(
            state.slash_suggestions().is_empty(),
            "a space after the command name closes the completion menu"
        );
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].role, MessageRole::User);
        assert_eq!(state.messages[0].content, "/help me");
        assert_eq!(state.messages[1].role, MessageRole::System);
        assert!(!state.pending_request);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handle_key_event_bang_prefix_executes_shell_command() {
        let mut state = AppState {
            input: "!echo hi".to_string(),
            cursor: 8,
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert_eq!(
            rx.try_recv().expect("shell command must be sent"),
            OutboundControl::ShellCommand("echo hi".to_string())
        );
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::User);
        assert_eq!(state.messages[0].content, "!echo hi");
        assert!(state.input.is_empty());
        assert!(!state.pending_request);
    }

    #[test]
    fn handle_key_event_bang_only_reports_empty_shell_command() {
        let mut state = AppState {
            input: "!".to_string(),
            cursor: 1,
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::System);
        assert!(state.messages[0].content.contains("empty"));
        assert!(rx.try_recv().is_err(), "empty shell command is not sent");
        assert!(state.input.is_empty());
    }

    #[test]
    fn input_title_reflects_bang_prefix() {
        let state = AppState {
            input: "!pwd".to_string(),
            ..AppState::default()
        };
        // The title is derived from input content, not a separate flag.
        assert!(state.input.starts_with('!'));
    }

    #[test]
    fn input_cursor_moves_left_right() {
        let mut state = AppState {
            input: "abc".to_string(),
            cursor: 3,
            input_area_width: 80,
            ..AppState::default()
        };
        move_cursor_left(&mut state, false);
        assert_eq!(state.cursor, 2);
        move_cursor_right(&mut state, false);
        assert_eq!(state.cursor, 3);
    }

    #[test]
    fn input_insert_and_delete_at_cursor() {
        let mut state = AppState {
            input: "ac".to_string(),
            cursor: 1,
            ..AppState::default()
        };
        insert_char(&mut state, 'b');
        assert_eq!(state.input, "abc");
        assert_eq!(state.cursor, 2);
        delete_back(&mut state);
        assert_eq!(state.input, "ac");
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn input_home_end() {
        let mut state = AppState {
            input: "hello world".to_string(),
            cursor: 5,
            input_area_width: 80,
            ..AppState::default()
        };
        move_cursor_home(&mut state, false);
        assert_eq!(state.cursor, 0);
        move_cursor_end(&mut state, true);
        assert_eq!(state.cursor, state.input.len());
    }

    #[test]
    fn input_cursor_wraps_lines() {
        let mut state = AppState {
            input: "abcd efgh".to_string(),
            cursor: 0,
            input_area_width: 5,
            ..AppState::default()
        };
        // "abcd " is 5 chars wide, "efgh" on next line.
        move_cursor_end(&mut state, false);
        assert_eq!(state.cursor, 5);
        move_cursor_vertical(&mut state, false);
        assert_eq!(state.cursor, 9);
    }

    #[test]
    fn parse_segments_splits_text_and_think_blocks() {
        let segments = parse_message_segments("hello <think>reasoning</think> world");
        assert_eq!(
            segments,
            vec![
                MessageSegment::Text("hello "),
                MessageSegment::Think {
                    index: 0,
                    text: "reasoning"
                },
                MessageSegment::Text(" world"),
            ]
        );
    }

    #[test]
    fn parse_segments_treats_unclosed_think_as_reasoning() {
        let segments = parse_message_segments("<think>still thinking");
        assert_eq!(
            segments,
            vec![MessageSegment::Think {
                index: 0,
                text: "still thinking"
            }]
        );
    }

    #[test]
    fn parse_segments_ignores_empty_think_block() {
        let segments = parse_message_segments("before<think></think>after");
        assert_eq!(
            segments,
            vec![
                MessageSegment::Text("before"),
                MessageSegment::Text("after"),
            ]
        );
    }

    #[test]
    fn parse_segments_handles_leading_think_block() {
        let segments = parse_message_segments("<think>reason</think>answer");
        assert_eq!(
            segments,
            vec![
                MessageSegment::Think {
                    index: 0,
                    text: "reason"
                },
                MessageSegment::Text("answer"),
            ]
        );
    }

    #[test]
    fn handle_key_event_page_up_down_scrolls_by_page() {
        let mut state = AppState {
            visible_chat_lines: 5,
            max_scroll: 20,
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageDown), &tx);
        assert_eq!(state.scroll, 5);

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageDown), &tx);
        assert_eq!(state.scroll, 10);

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageUp), &tx);
        assert_eq!(state.scroll, 5);
    }

    #[test]
    fn handle_key_event_ctrl_home_end_jumps_to_top_bottom() {
        let mut state = AppState {
            visible_chat_lines: 5,
            max_scroll: 30,
            scroll: 7,
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(
            &mut state,
            event::KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::empty(),
            },
            &tx,
        );
        assert_eq!(state.scroll, 30);

        handle_key_event(
            &mut state,
            event::KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::empty(),
            },
            &tx,
        );
        assert_eq!(state.scroll, 0);
    }

    fn ctrl_key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    #[test]
    fn handle_key_event_bare_letters_reach_the_input() {
        // Regression: the input box is always focused, so bare 't'/'q' must
        // type into it rather than trigger the thinking/quit shortcuts.
        let mut state = AppState::default();
        state.messages.push(ChatMessage::new(
            MessageRole::Assistant,
            "<think>secret</think>answer".to_string(),
        ));
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('t')), &tx);
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('q')), &tx);

        assert_eq!(state.input, "tq");
        assert!(!state.quit);
        assert!(state.expanded_thinks.is_empty());
    }

    #[test]
    fn handle_key_event_ctrl_q_quits() {
        let mut state = AppState::default();
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, ctrl_key(KeyCode::Char('q')), &tx);

        assert!(state.quit);
    }

    #[test]
    fn handle_key_event_ctrl_t_toggles_thinking() {
        let mut state = AppState::default();
        state.messages.push(ChatMessage::new(
            MessageRole::Assistant,
            "<think>secret</think>answer".to_string(),
        ));
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, ctrl_key(KeyCode::Char('t')), &tx);
        assert!(state.expanded_thinks.contains(&(0, 0)));
        assert!(state.input.is_empty());

        handle_key_event(&mut state, ctrl_key(KeyCode::Char('t')), &tx);
        assert!(state.expanded_thinks.is_empty());
    }

    #[test]
    fn handle_mouse_event_scroll_wheel_scrolls() {
        let mut state = AppState {
            visible_chat_lines: 5,
            max_scroll: 20,
            scroll: 10,
            ..AppState::default()
        };

        handle_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert_eq!(state.scroll, 13);

        handle_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert_eq!(state.scroll, 10);
    }

    #[test]
    fn scrolling_never_goes_negative() {
        let mut state = AppState {
            visible_chat_lines: 5,
            max_scroll: 20,
            scroll: 1,
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();

        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageUp), &tx);
        assert_eq!(state.scroll, 0);

        handle_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn apply_scroll_follows_when_at_bottom() {
        let mut state = AppState {
            scroll: 10,
            max_scroll: 10,
            ..AppState::default()
        };
        apply_scroll(&mut state, 20);
        assert_eq!(state.scroll, 20);
        assert!(!state.force_scroll_bottom);
    }

    #[test]
    fn apply_scroll_preserves_manual_position() {
        let mut state = AppState {
            scroll: 5,
            max_scroll: 20,
            ..AppState::default()
        };
        apply_scroll(&mut state, 25);
        assert_eq!(state.scroll, 5);
    }

    #[test]
    fn apply_scroll_clamps_when_content_shrinks() {
        let mut state = AppState {
            scroll: 15,
            max_scroll: 20,
            ..AppState::default()
        };
        apply_scroll(&mut state, 10);
        assert_eq!(state.scroll, 10);
    }

    #[test]
    fn apply_scroll_forces_bottom_when_flag_set() {
        let mut state = AppState {
            scroll: 2,
            max_scroll: 20,
            force_scroll_bottom: true,
            ..AppState::default()
        };
        apply_scroll(&mut state, 30);
        assert_eq!(state.scroll, 30);
        assert!(!state.force_scroll_bottom);
    }

    #[test]
    fn handle_key_event_enter_forces_scroll_bottom() {
        let mut state = AppState {
            input: "hi".to_string(),
            cursor: 2,
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert!(state.force_scroll_bottom);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn handle_paste_inserts_small_text() {
        let mut state = AppState::default();
        handle_paste(&mut state, "hello".to_string());
        assert_eq!(state.input, "hello");
        assert_eq!(state.cursor, 5);
        assert!(state.paste_store.is_empty());
    }

    #[test]
    fn handle_paste_inserts_multiline_text_under_threshold() {
        let mut state = AppState::default();
        let text = "line1\nline2\nline3".to_string();
        handle_paste(&mut state, text.clone());
        assert_eq!(state.input, text);
        assert!(state.paste_store.is_empty());
    }

    #[test]
    fn handle_paste_collapses_long_text() {
        let mut state = AppState::default();
        let text = "x".repeat(PASTE_CHAR_THRESHOLD + 1);
        handle_paste(&mut state, text.clone());
        assert!(!state.input.contains(&text));
        assert!(state.input.contains("Pasted text"));
        assert!(state.input.contains(&format!("{} chars", text.len())));
        assert_eq!(state.paste_store.len(), 1);
        assert!(state.paste_store.values().any(|v| v == &text));
    }

    #[test]
    fn handle_paste_collapses_many_lines() {
        let mut state = AppState::default();
        let text = (0..PASTE_LINE_THRESHOLD + 1)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        handle_paste(&mut state, text.clone());
        assert!(state.input.contains("Pasted text"));
        assert!(
            state
                .input
                .contains(&format!("{} lines", PASTE_LINE_THRESHOLD + 1))
        );
        assert_eq!(state.paste_store.len(), 1);
    }

    #[test]
    fn expand_paste_placeholders_restores_original_text() {
        let mut store = HashMap::new();
        store.insert(
            "[...Pasted text #0: 3 lines, 12 chars...]".to_string(),
            "a\nb\nc".to_string(),
        );
        let input = "before [...Pasted text #0: 3 lines, 12 chars...] after".to_string();
        let expanded = expand_paste_placeholders(&input, &store);
        assert_eq!(expanded, "before a\nb\nc after");
    }

    #[test]
    fn enter_sends_expanded_paste_content() {
        let mut state = AppState::default();
        let content = "x".repeat(PASTE_CHAR_THRESHOLD + 1);
        handle_paste(&mut state, content.clone());
        assert_eq!(state.paste_store.len(), 1);

        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        let sent = rx.try_recv().expect("expected a message to be sent");
        assert_eq!(sent, OutboundControl::Message(content));
        assert!(state.input.is_empty());
        assert!(state.paste_store.is_empty());
    }

    #[test]
    fn tui_session_key_includes_unique_peer_id() {
        let key1 = tui_session_key("abc");
        let key2 = tui_session_key("def");
        assert!(key1.starts_with("agent:main:dm:tui:default:direct:"));
        assert!(key2.starts_with("agent:main:dm:tui:default:direct:"));
        assert_ne!(key1, key2);
    }

    #[test]
    fn history_messages_from_payload_keeps_user_and_assistant_text() {
        let resp = json!({
            "payload": {
                "sessionKey": "agent:main:dm:tui:default:direct:u1",
                "messages": [
                    { "role": "user", "content": "hi" },
                    { "role": "assistant", "content": "hello" },
                    { "role": "assistant", "content": "", "tool_calls": [{"id": "c1"}] },
                    { "role": "tool", "content": "file list...", "tool_call_id": "c1" },
                    { "role": "system", "content": "summary" }
                ]
            }
        });
        let msgs = history_messages_from_payload(&resp);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[1].content, "hello");
    }

    #[test]
    fn history_messages_from_payload_tolerates_missing_payload() {
        assert!(history_messages_from_payload(&json!({})).is_empty());
        assert!(history_messages_from_payload(&json!({"payload": {}})).is_empty());
    }

    #[test]
    fn uuid_v4_is_well_formed_and_unique() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        for (i, c) in a.char_indices() {
            match i {
                8 | 13 | 18 | 23 => assert_eq!(c, '-'),
                14 => assert_eq!(c, '4', "version nibble"),
                19 => assert!(matches!(c, '8' | '9' | 'a' | 'b'), "variant nibble"),
                _ => assert!(c.is_ascii_hexdigit(), "non-hex char {c} at {i}"),
            }
        }
        // UUID chars (hex + '-') all pass the peer-id whitelist.
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }

    #[test]
    fn user_message_renders_immediately_after_enter() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = AppState {
            input: "hello".to_string(),
            cursor: 5,
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_ui(f, &mut state)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            text.contains("hello"),
            "user message should be rendered immediately; buffer:\n{}",
            text
        );
        // No empty assistant placeholder should be created yet; the status bar
        // still indicates that a request is in flight.
        assert!(
            !text.contains("Legion:"),
            "assistant placeholder should not be rendered before first token; buffer:\n{}",
            text
        );
        assert!(
            text.contains("typing..."),
            "status bar should show typing indicator; buffer:\n{}",
            text
        );
    }

    #[test]
    fn user_message_visible_in_small_viewport_after_enter() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = AppState {
            input: "hello world".to_string(),
            cursor: 11,
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        // Very small terminal: chat area height = 5 - 2 borders = 3 visible lines.
        // The new role bar widens the prefix, so the message may wrap; we just
        // verify every part of it is still visible.
        let backend = TestBackend::new(20, 11);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_ui(f, &mut state)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            text.contains("hello") && text.contains("world"),
            "user message should remain visible even in small viewport; buffer:\n{}",
            text
        );
    }

    #[test]
    fn last_message_visible_when_content_wraps() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Fill the viewport with previous assistant content so the new user
        // message sits at the bottom.
        let mut state = AppState {
            messages: vec![ChatMessage::new(
                MessageRole::Assistant,
                "first line\nsecond line".to_string(),
            )],
            input: "this is a long user message".to_string(),
            cursor: 27,
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<OutboundControl>();
        handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        // Width 20, height 12 => chat area height 5 => 3 visible lines.
        // The long user message plus prefix wraps, so correct scroll math is
        // required to keep the tail visible.
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_ui(f, &mut state)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            text.contains("You:") && text.contains("this is a long") && text.contains("message"),
            "last wrapped user message should be visible; buffer:\n{}",
            text
        );
    }

    fn sample_pending_question(multi_select: bool) -> PendingQuestion {
        PendingQuestion {
            prompt_id: "prompt-1".to_string(),
            questions: vec![
                AskUserQuestion {
                    question: "Which color?".to_string(),
                    header: "Color".to_string(),
                    options: vec![
                        AskUserOption {
                            label: "Red".to_string(),
                            description: "Warm".to_string(),
                            preview: None,
                        },
                        AskUserOption {
                            label: "Blue".to_string(),
                            description: "Cool".to_string(),
                            preview: None,
                        },
                    ],
                    multi_select,
                },
                AskUserQuestion {
                    question: "Which size?".to_string(),
                    header: "Size".to_string(),
                    options: vec![
                        AskUserOption {
                            label: "Small".to_string(),
                            description: "Compact".to_string(),
                            preview: None,
                        },
                        AskUserOption {
                            label: "Large".to_string(),
                            description: "Spacious".to_string(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                },
            ],
            current: 0,
            selected_labels: HashMap::new(),
            focused: 0,
            message_index: 0,
        }
    }

    #[test]
    fn question_renders_question_tabs_with_vertical_options() {
        let pq = sample_pending_question(false);
        let msg = format_question_message(&pq);
        assert!(
            msg.contains("> Color <"),
            "first question tab should be focused: {msg}"
        );
        assert!(
            msg.contains("[ Size ]"),
            "second question tab should be present: {msg}"
        );
        assert!(
            msg.contains("[ Submit ]"),
            "final Submit tab should be present: {msg}"
        );
        assert!(
            msg.contains("> ( ) Red"),
            "options should be listed vertically with focused indicator: {msg}"
        );
        assert!(
            msg.contains("Warm"),
            "focused option description should be shown: {msg}"
        );
    }

    #[test]
    fn question_left_right_switch_tabs_and_up_down_select_options() {
        let mut state = AppState::default();
        state
            .messages
            .push(ChatMessage::new(MessageRole::Question, String::new()));
        state.pending_question = Some(sample_pending_question(false));
        let (tx, _rx) = mpsc::unbounded_channel::<OutboundControl>();

        // Tab count = 2 questions + Submit = 3. current starts at 0 (Color).
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Left), &tx);
        assert_eq!(
            state.pending_question.as_ref().unwrap().current,
            2,
            "left from first tab should wrap to Submit"
        );

        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
        assert_eq!(
            state.pending_question.as_ref().unwrap().current,
            0,
            "right from Submit should wrap to first question"
        );

        // Up/Down navigate options within the current question.
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(
            state.pending_question.as_ref().unwrap().focused,
            1,
            "down should move to Blue option"
        );

        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
        assert_eq!(
            state.pending_question.as_ref().unwrap().current,
            1,
            "right should switch to Size question"
        );
        assert_eq!(
            state.pending_question.as_ref().unwrap().focused,
            0,
            "focused option should reset when switching tabs"
        );
    }

    #[test]
    fn question_enter_selects_option_then_submit_tab_submits() {
        let mut state = AppState::default();
        state
            .messages
            .push(ChatMessage::new(MessageRole::Question, String::new()));
        state.pending_question = Some(sample_pending_question(false));
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundControl>();

        // Select Red on the Color question.
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        {
            let pq = state.pending_question.as_ref().unwrap();
            assert!(
                pq.is_selected("Which color?", "Red"),
                "Red should be selected"
            );
            assert!(
                !pq.is_selected("Which color?", "Blue"),
                "Blue should not be selected"
            );
        }

        // Switch to Size question and select Large.
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        {
            let pq = state.pending_question.as_ref().unwrap();
            assert!(
                pq.is_selected("Which size?", "Large"),
                "Large should be selected"
            );
        }

        // Move to Submit tab and confirm.
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
        assert!(state.pending_question.as_ref().unwrap().is_submit_tab());
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        assert!(
            state.pending_question.is_none(),
            "prompt should be resolved after submitting"
        );
        let sent = rx.try_recv().expect("answer should be sent");
        match sent {
            OutboundControl::ResolveQuestion { output, .. } => {
                assert_eq!(output.answers.get("Which color?"), Some(&"Red".to_string()));
                assert_eq!(
                    output.answers.get("Which size?"),
                    Some(&"Large".to_string())
                );
            }
            other => panic!("expected ResolveQuestion, got {other:?}"),
        }
    }

    #[test]
    fn question_space_toggles_multi_select_within_question() {
        let mut state = AppState::default();
        state
            .messages
            .push(ChatMessage::new(MessageRole::Question, String::new()));
        state.pending_question = Some(sample_pending_question(true));
        let (tx, _rx) = mpsc::unbounded_channel::<OutboundControl>();

        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Char(' ')), &tx);
        assert!(
            state
                .pending_question
                .as_ref()
                .unwrap()
                .is_selected("Which color?", "Red")
        );

        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Char(' ')), &tx);
        {
            let pq = state.pending_question.as_ref().unwrap();
            assert!(
                pq.is_selected("Which color?", "Red"),
                "Red should stay selected"
            );
            assert!(
                pq.is_selected("Which color?", "Blue"),
                "Blue should be selected"
            );
        }
    }

    #[test]
    fn todo_panel_shows_active_and_completed_items() {
        let state = AppState {
            todos: vec![
                TodoItem {
                    id: "1".into(),
                    content: "Plan migration".into(),
                    status: TodoStatus::InProgress,
                    active_form: "Planning migration".into(),
                },
                TodoItem {
                    id: "2".into(),
                    content: "Run tests".into(),
                    status: TodoStatus::Completed,
                    active_form: String::new(),
                },
            ],
            todo_max_display: 6,
            ..AppState::default()
        };

        let lines = render_todo_panel(&state, 40);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("■ Plan migration"));
        assert!(text.contains("✓ Run tests"));
    }

    #[test]
    fn todo_update_overwrites_items_and_schedules_hide() {
        let mut state = AppState {
            todos: vec![TodoItem {
                id: "1".into(),
                content: "Old".into(),
                status: TodoStatus::InProgress,
                active_form: String::new(),
            }],
            todo_auto_hide_seconds: 5,
            ..AppState::default()
        };

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": {
                "stream": "todo_update",
                "items": [
                    {"id": "1", "content": "Updated", "status": "in_progress", "activeForm": ""},
                    {"id": "2", "content": "New", "status": "completed", "activeForm": ""}
                ]
            }
        });
        handle_ws_event(&mut state, event);

        assert_eq!(state.todos.len(), 2);
        assert_eq!(state.todos[0].content, "Updated");
        assert_eq!(state.todos[1].status, TodoStatus::Completed);
        assert!(
            state.todo_hide_at.is_none(),
            "hide only scheduled when every item is completed"
        );

        let all_done = json!({
            "type": "event",
            "event": "agent",
            "payload": {
                "stream": "todo_update",
                "items": [
                    {"id": "1", "content": "Updated", "status": "completed", "activeForm": ""},
                    {"id": "2", "content": "New", "status": "completed", "activeForm": ""}
                ]
            }
        });
        handle_ws_event(&mut state, all_done);
        assert!(state.todo_hide_at.is_some(), "hide scheduled when all done");
    }

    #[test]
    fn todo_update_empty_list_clears_todos() {
        let mut state = AppState {
            todos: vec![TodoItem {
                id: "1".into(),
                content: "Stale".into(),
                status: TodoStatus::InProgress,
                active_form: String::new(),
            }],
            ..AppState::default()
        };

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "todo_update", "items": [] }
        });
        handle_ws_event(&mut state, event);

        assert!(state.todos.is_empty());
        assert!(state.todo_hide_at.is_none());
    }

    #[test]
    fn truncate_to_width_respects_display_width() {
        assert_eq!(truncate_to_width("hello", 5), "hello");
        assert_eq!(truncate_to_width("hello world", 5), "hell…");
        // CJK characters are roughly 2 display columns each.
        assert_eq!(truncate_to_width("中文", 3), "中…");
        assert_eq!(truncate_to_width("中文", 4), "中文");
    }
}
