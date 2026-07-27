//! TUI state types and structures.

use crate::tui::composer::Composer;
use crate::tui::history_search::HistorySearch;
use crate::tui::selection::Selection;
use crate::tui::syntax::Highlighter;
use crate::tui::theme::Theme;
use legion_runtime::{AskUserOutput, AskUserQuestion, TodoItem};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::Line;
use std::collections::{HashMap, HashSet};

/// Pastes longer than this many characters are collapsed into a placeholder.
pub(crate) const PASTE_CHAR_THRESHOLD: usize = 1000;
/// Pastes with more than this many lines are collapsed into a placeholder.
pub(crate) const PASTE_LINE_THRESHOLD: usize = 10;
/// Head lines kept when a tool-card result section (stdout/stderr/text) is truncated.
pub(crate) const TOOL_RESULT_HEAD_LINES: usize = 25;
/// Tail lines kept when a tool-card result section is truncated.
pub(crate) const TOOL_RESULT_TAIL_LINES: usize = 10;
/// Maximum characters of a tool call's arguments shown on the card.
pub(crate) const TOOL_ARGS_MAX_CHARS: usize = 500;

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

/// Display mode for the TUI.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum ScreenMode {
    /// Traditional full-screen alternate-buffer mode (default).
    #[default]
    Fullscreen,
    /// Inline live viewport drawn at the bottom of the normal scrollback.
    Inline,
}

impl ScreenMode {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "fullscreen" => Some(Self::Fullscreen),
            "inline" => Some(Self::Inline),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Fullscreen => "fullscreen",
            Self::Inline => "inline",
        }
    }
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
    pub(crate) fn new(role: MessageRole, content: impl Into<String>) -> Self {
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
pub(crate) enum OutboundControl {
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
}

#[derive(Default, Clone)]
pub struct AppState {
    pub(crate) messages: Vec<ChatMessage>,
    /// Rich multi-line input editor.
    pub(crate) composer: Composer,
    /// User inputs sent in this TUI session, recalled with ↑/↓.
    pub(crate) input_history: Vec<String>,
    /// Index into `input_history` when browsing history. `None` means the
    /// current draft is being edited.
    pub(crate) history_index: Option<usize>,
    /// Draft saved when the user first presses ↑, restored by ↓ at the
    /// newest history entry.
    pub(crate) draft_input: Option<String>,
    pub(crate) status: String,
    pub theme: Theme,
    pub highlighter: Highlighter,
    pub(crate) scroll: usize,
    pub(crate) quit: bool,
    /// Selected index in the slash-command completion menu.
    pub(crate) slash_selected: usize,
    /// Peer id of the current session, shown by `/status` and reused by the
    /// exit-time resume hint.
    pub(crate) session_peer: String,
    /// Which `(message_index, think_index)` blocks are expanded.
    pub(crate) expanded_thinks: HashSet<(usize, usize)>,
    /// Cached input area width (inner) for dynamic input height calculations.
    pub(crate) input_area_width: u16,
    /// Cached viewport height for scroll clamping.
    pub(crate) viewport_height: u16,
    /// Cached visible chat lines for page scrolling.
    pub(crate) visible_chat_lines: u16,
    /// Cached maximum scroll position (updated each draw).
    pub(crate) max_scroll: usize,
    /// Cached chat area rectangle, used to map mouse events to scrollback positions.
    pub(crate) chat_area: Rect,
    /// If true, the next draw should snap the message list to the bottom.
    pub(crate) force_scroll_bottom: bool,
    /// True while a user request has been sent but the run has not finished.
    /// Used to keep the status bar in "typing..." state before the first token
    /// arrives, without adding an empty assistant placeholder that would push
    /// the user's own message out of the viewport.
    pub(crate) pending_request: bool,
    /// A tool-approval prompt awaiting the user's y/n answer:
    /// `(prompt_id, tool)`. While set, key input is intercepted by the
    /// approval handler instead of reaching the input box.
    pub(crate) pending_approval: Option<(String, String)>,
    /// An `ask_user` question prompt awaiting the user's answer. While set,
    /// key input is intercepted by the question handler.
    pub(crate) pending_question: Option<PendingQuestion>,
    /// Screen rectangles of thinking hint lines for mouse clicks.
    pub(crate) think_hitboxes: Vec<(ratatui::layout::Rect, usize, usize)>,
    /// Screen rectangles of each visible message body, refreshed each draw, as
    /// `(msg_idx, rect, first_line)`. `first_line` is the index into the
    /// message's rendered lines of the row at `rect.y` (nonzero when the
    /// message's top is scrolled out of view). Drives click→cursor mapping via
    /// `Rect::contains` — the rendered geometry is the single source of truth,
    /// with no parallel line-number arithmetic (mirrors `think_hitboxes`; same
    /// idea as grok-build's `HitArea`).
    pub(crate) message_rects: Vec<(usize, ratatui::layout::Rect, usize)>,
    /// Current scrollback text selection, if any.
    pub(crate) selection: Option<Selection>,
    /// Whether the user is currently dragging to select text.
    pub(crate) selecting: bool,
    /// History search popup state.
    pub(crate) history_search: Option<HistorySearch>,
    /// Stored pasted content keyed by placeholder token.
    pub(crate) paste_store: HashMap<String, String>,
    /// Next placeholder id for pasted content.
    pub(crate) next_paste_id: u64,
    /// Per-message render cache, parallel to `messages`. Entries are
    /// re-rendered lazily by `ensure_render_cache` when their inputs change.
    pub(crate) render_cache: Vec<Option<CachedRender>>,
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
    /// Full-screen or inline viewport mode.
    pub screen_mode: ScreenMode,
    /// Name of the active theme (for `/status` and persistence).
    pub(crate) theme_name: String,
    /// Config file path used to persist `/theme` and `/mode`. `None` in
    /// tests, where persistence must not touch the real config file.
    pub(crate) config_path: Option<std::path::PathBuf>,
    /// In inline mode, index of the last message already emitted to the native
    /// scrollback. Messages finalized after this index are flushed on the next
    /// frame.
    pub last_emitted_scrollback_index: usize,
}

/// UI state for an in-flight `ask_user` prompt.
#[derive(Clone)]
pub(crate) struct PendingQuestion {
    pub(crate) prompt_id: String,
    pub(crate) questions: Vec<AskUserQuestion>,
    /// Index of the currently visible tab. Tabs are the questions followed by
    /// a final "Submit" tab, so valid indices are `0..=questions.len()`.
    pub(crate) current: usize,
    /// Selected answer labels per question text.
    pub(crate) selected_labels: HashMap<String, HashSet<String>>,
    /// Focused option index within the current *question* tab. Only meaningful
    /// when `current < questions.len()`.
    pub(crate) focused: usize,
    /// Index of the inline message in `AppState.messages` that shows the prompt.
    pub(crate) message_index: usize,
}

/// Label shown on the final confirmation tab.
pub(crate) const SUBMIT_LABEL: &str = "Submit";

impl PendingQuestion {
    pub(crate) fn current_question(&self) -> Option<&AskUserQuestion> {
        self.questions.get(self.current)
    }

    pub(crate) fn is_multi_select(&self) -> bool {
        self.current_question().is_some_and(|q| q.multi_select)
    }

    /// Total number of tabs: one per question plus Submit.
    pub(crate) fn tab_count(&self) -> usize {
        self.questions.len() + 1
    }

    /// Returns `true` when the focus is on the final Submit tab.
    pub(crate) fn is_submit_tab(&self) -> bool {
        self.current == self.questions.len()
    }

    pub(crate) fn is_selected(&self, question: &str, label: &str) -> bool {
        self.selected_labels
            .get(question)
            .is_some_and(|s| s.contains(label))
    }

    pub(crate) fn toggle(&mut self, question: &str, label: &str) {
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

    pub(crate) fn select_only(&mut self, question: &str, label: &str) {
        self.selected_labels.insert(question.to_string(), {
            let mut s = HashSet::new();
            s.insert(label.to_string());
            s
        });
    }

    pub(crate) fn into_output(self) -> AskUserOutput {
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

impl AppState {
    /// Bring `render_cache` up to date with `messages` for the given viewport
    /// width. Only messages whose rendered inputs changed (content, state,
    /// expanded thinking blocks, width) are re-rendered; everything else is
    /// reused, so a steady-state frame costs ~nothing for old history.
    pub(crate) fn ensure_render_cache(&mut self, width: u16) {
        use crate::tui::render::{render_key, wrap_and_remap};
        use crate::tui::widgets::{left_bar_span, message_lines, role_background};
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
                &self.theme,
                &self.highlighter,
            );
            let (mut lines, think_hints) = wrap_and_remap(rendered, content_width);
            if role != MessageRole::Tool {
                let bar = left_bar_span(role, &self.theme);
                let bg = role_background(role, &self.theme);
                if bg != Color::Reset {
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
    pub(crate) fn cached_total_lines(&self) -> usize {
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
    pub(crate) fn is_active(&self) -> bool {
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
    pub(crate) fn page_scroll_delta(&self) -> usize {
        let lines = self.visible_chat_lines as usize;
        if lines == 0 { 10 } else { lines }
    }

    /// Completion candidates for the current input, or an empty list when the
    /// input is not a bare command name. Mirrors Claude Code's rule: a
    /// whitespace after the command name (i.e. arguments) closes the menu.
    pub(crate) fn slash_suggestions(&self) -> Vec<crate::slash_commands::SlashCommand> {
        let text = self.composer.join();
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
            crate::slash_commands::suggestions(&text[1..], &self.loaded_skills)
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
        self.message_rects.clear();
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

pub(crate) struct RenderedMessage {
    pub(crate) lines: Vec<Line<'static>>,
    /// Hint line numbers index the *unwrapped* `lines`; `wrap_and_remap`
    /// translates them into wrapped-line space for the cache.
    pub(crate) think_hints: Vec<ThinkHint>,
}

#[derive(Clone)]
pub(crate) struct ThinkHint {
    pub(crate) block_index: usize,
    pub(crate) start_line: usize,
    pub(crate) line_count: usize,
}

/// Fingerprint of everything a message's rendered output depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RenderKey {
    pub(crate) content_hash: u64,
    pub(crate) state: MessageState,
    /// Hash of the sorted expanded thinking-block indices for this message.
    pub(crate) expanded_hash: u64,
    pub(crate) width: u16,
}

/// Per-message cached render output: fully wrapped lines plus thinking
/// hints translated into wrapped-line space, relative to the message start.
#[derive(Clone)]
pub(crate) struct CachedRender {
    pub(crate) key: RenderKey,
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) think_hints: Vec<ThinkHint>,
}
