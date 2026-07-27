//! Interactive TUI for Legion, similar to Claude Code.

mod ansi;
mod composer;
mod events;
mod history_search;
mod inline;
mod input;
mod links;
mod markdown;
mod question;
mod render;
mod selection;
mod state;
pub mod syntax;
pub mod theme;
mod tool_card;
mod widgets;
mod writer;

pub use state::{AppState, ChatMessage, MessageRole, MessageState, ScreenMode};

use crate::driver::{
    CliMode, EMBEDDED_NOTICE, LocalDriver, TurnDriver, WsDriver, build_local_host, probe_gateway,
    session_cron_store_path,
};
use crate::{CliError, GatewayClient, load_config};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use inline::{INLINE_HEIGHT, reset_emitted_index};
use legion_core::util::lock_recover;
use legion_skills::SkillRegistry;
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::CrosstermBackend;
use serde_json::json;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use writer::{TermWriter, WriterThread};

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
    let mut state_inner = state::AppState {
        todo_auto_hide_seconds: config.todos.auto_hide_seconds,
        todo_max_display: config.todos.max_display,
        session_key: session_key.clone(),
        goal_store: goal_store.clone(),
        ..state::AppState::default()
    };
    state_inner
        .composer
        .placeholder("type a message · / commands · ! shell");

    // Apply persisted TUI preferences.
    match crate::tui::theme::Theme::by_name(&config.tui.theme) {
        Some(theme) => {
            state_inner.theme = theme;
            state_inner.theme_name = config.tui.theme.clone();
        }
        None => {
            tracing::warn!(theme = %config.tui.theme, "unknown TUI theme in config; using default");
            state_inner.theme_name = "default".to_string();
        }
    }
    if let Some(mode) = state::ScreenMode::from_name(&config.tui.screen_mode) {
        state_inner.screen_mode = mode;
    } else {
        tracing::warn!(mode = %config.tui.screen_mode, "unknown TUI screen mode in config; using fullscreen");
    }
    state_inner.config_path = crate::default_config_path();

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

    // Local TUI sessions use a scoped cron store so `/loop` and
    // `scheduler_create` write to the same session-level JSONL file.
    let local_cron_store_path = session_cron_store_path(&session_key);

    // Select the transport. The WebSocket path behaves exactly as before
    // (Gateway mode still auto-starts the gateway); Auto probes briefly and
    // falls back to an embedded runtime without starting anything.
    let mut version_warning: Option<String> = None;
    let driver: Arc<dyn TurnDriver> = match mode {
        CliMode::Local => Arc::new(
            LocalDriver::new(
                Arc::new(build_local_host(&config, local_cron_store_path.clone()).await?),
                session_key.clone(),
                event_tx.clone(),
                yolo,
                workspace_override.clone(),
            )
            .await?,
        ),
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
                Arc::new(
                    LocalDriver::new(
                        Arc::new(build_local_host(&config, local_cron_store_path.clone()).await?),
                        session_key.clone(),
                        event_tx.clone(),
                        yolo,
                        workspace_override.clone(),
                    )
                    .await?,
                )
            }
        },
    };

    lock_recover(&state).status = driver.mode_name().to_string();

    // On a fresh session show a short welcome instead of a bare workspace hint.
    if !resuming {
        let mode_name = driver.mode_name();
        let ws = workspace_override
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "config default".to_string());
        lock_recover(&state).messages.push(state::ChatMessage::new(
            state::MessageRole::System,
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
        lock_recover(&state).messages.push(state::ChatMessage::new(
            state::MessageRole::System,
            format!("workspace: {ws}"),
        ));
    }
    if let Some(warning) = version_warning {
        state
            .lock()
            .unwrap()
            .messages
            .push(state::ChatMessage::new(state::MessageRole::System, warning));
    }
    if yolo {
        lock_recover(&state).messages.push(state::ChatMessage::new(
            state::MessageRole::System,
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
                let resumed = events::history_messages_from_payload(&resp);
                {
                    let mut s = lock_recover(&state);
                    for msg in &resumed {
                        if msg.role == state::MessageRole::User {
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
                lock_recover(&state).messages.push(state::ChatMessage::new(
                    state::MessageRole::System,
                    format!(
                        "failed to load session history: {err} \
                         (stale gateway? restart with `legion gateway stop && legion gateway start`)"
                    ),
                ));
            }
            Ok(Err(err)) => {
                lock_recover(&state).messages.push(state::ChatMessage::new(
                    state::MessageRole::System,
                    format!("failed to load session history: {err}"),
                ));
            }
            Err(_) => {
                lock_recover(&state).messages.push(state::ChatMessage::new(
                    state::MessageRole::System,
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
    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<state::OutboundControl>();
    let sender_state = state.clone();
    let sender_driver = Arc::clone(&driver);
    // The sender task uses this clone to wake the UI loop when it mutates
    // state outside of the event flow (the loop only redraws on events).
    let wake_tx = event_tx.clone();
    // The sender task below moves `session_key`; keep the peer id for the
    // exit-time resume hint. `/status` shows the same value.
    let peer_id = crate::session_peer_id(&session_key).to_string();
    lock_recover(&state).session_peer = peer_id.clone();
    tokio::spawn(async move {
        while let Some(command) = send_rx.recv().await {
            match command {
                state::OutboundControl::Message(text) => {
                    // The active goal is injected by the runtime at run start
                    // (agent_loop), so the text goes out unchanged here.
                    if let Err(err) = sender_driver.run_turn(text).await {
                        let mut s = lock_recover(&sender_state);
                        s.messages.push(state::ChatMessage::new(
                            state::MessageRole::System,
                            format!("failed to send: {err}"),
                        ));
                        s.messages.last_mut().unwrap().state = state::MessageState::Error;
                        s.pending_request = false;
                        drop(s);
                        let _ = wake_tx.send(json!({ "type": "internal", "event": "send-failed" }));
                    }
                }
                state::OutboundControl::ShellCommand(command) => {
                    let output = crate::shell_commands::run_shell_command(&command).await;
                    let mut s = lock_recover(&sender_state);
                    s.messages
                        .push(state::ChatMessage::new(state::MessageRole::System, output));
                    drop(s);
                    let _ = wake_tx.send(json!({ "type": "internal", "event": "shell-done" }));
                }
                state::OutboundControl::ResolveApproval { prompt_id, allow } => {
                    sender_driver.resolve_approval(&prompt_id, allow).await;
                }
                state::OutboundControl::ResolveQuestion { prompt_id, output } => {
                    sender_driver.resolve_question(&prompt_id, output).await;
                }
                state::OutboundControl::Cancel => {
                    if let Err(err) = sender_driver.cancel().await {
                        let mut s = lock_recover(&sender_state);
                        s.messages.push(state::ChatMessage::new(
                            state::MessageRole::System,
                            format!("{err}"),
                        ));
                        drop(s);
                        let _ =
                            wake_tx.send(json!({ "type": "internal", "event": "cancel-failed" }));
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

/// Result of one `tui_loop` invocation.
enum LoopOutcome {
    /// The user asked to quit.
    Quit,
    /// The viewport mode changed; the terminal should be recreated.
    ModeSwitch,
}

async fn run_terminal(
    state: Arc<Mutex<state::AppState>>,
    send_tx: mpsc::UnboundedSender<state::OutboundControl>,
    event_rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
) -> Result<(), CliError> {
    let mut current_mode = lock_recover(&state).screen_mode;

    loop {
        enable_raw_mode()?;
        let mut stdout = io::stdout();

        if current_mode == ScreenMode::Fullscreen {
            // Enable mouse capture so the scroll wheel works. Terminal
            // emulators follow a universal convention: holding Shift
            // bypasses the app's mouse capture and falls back to native
            // text selection. This is how tmux, htop, less, and all
            // ratatui/crossterm apps reconcile "scroll wheel works" with
            // "user can still select text".
            crossterm::execute!(
                &mut stdout,
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture,
                EnableBracketedPaste
            )?;
        }

        let (term_writer, scrollback_tx, writer) = WriterThread::spawn(stdout);
        let backend = CrosstermBackend::new(term_writer);
        let mut terminal = match current_mode {
            ScreenMode::Fullscreen => Terminal::new(backend)?,
            ScreenMode::Inline => {
                // In inline mode the live viewport lives at the bottom of the
                // normal scrollback. Reset the scrollback emission cursor so
                // we do not dump pre-existing history onto the current line.
                reset_emitted_index(&mut lock_recover(&state));
                Terminal::with_options(
                    backend,
                    TerminalOptions {
                        viewport: Viewport::Inline(INLINE_HEIGHT),
                    },
                )?
            }
        };

        let outcome = tui_loop(
            &mut terminal,
            state.clone(),
            send_tx.clone(),
            event_rx,
            scrollback_tx,
        )
        .await;

        // Reverse fullscreen terminal setup before dropping the terminal.
        if current_mode == ScreenMode::Fullscreen {
            crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture,
                DisableBracketedPaste
            )?;
        }

        // Drop the terminal (which flushes its backend) and wait for the
        // writer thread to drain the last bytes before returning.
        drop(terminal);
        writer.join()?;
        disable_raw_mode()?;

        match outcome {
            Ok(LoopOutcome::ModeSwitch) => {
                current_mode = lock_recover(&state).screen_mode;
                continue;
            }
            Ok(LoopOutcome::Quit) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<TermWriter>>,
    state: Arc<Mutex<state::AppState>>,
    send_tx: mpsc::UnboundedSender<state::OutboundControl>,
    event_rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
    scrollback_tx: std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<LoopOutcome, CliError> {
    let mut last_tick = tokio::time::Instant::now();
    let tick_rate = Duration::from_millis(100);
    // Nothing on screen animates, so the UI only redraws when an event may
    // have changed state. Idle iterations block in `poll` and cost no CPU.
    let mut dirty = true;
    let initial_mode = lock_recover(&state).screen_mode;

    loop {
        // Drain incoming websocket events.
        let mut had_events = false;
        while let Ok(msg) = event_rx.try_recv() {
            // Goal mode: the model may have updated the session goal via the
            // goal tools during the run; refresh the TUI's copy when the run
            // finishes so the status bar and /goal reflect it.
            let run_finished = msg.get("event").and_then(|v| v.as_str()) == Some("agent")
                && msg
                    .get("payload")
                    .and_then(|p| p.get("stream"))
                    .and_then(|v| v.as_str())
                    == Some("lifecycle")
                && matches!(
                    msg.get("payload")
                        .and_then(|p| p.get("phase"))
                        .and_then(|v| v.as_str()),
                    Some("end") | Some("error")
                );
            events::handle_ws_event(&mut lock_recover(&state), msg, &send_tx);
            if run_finished {
                let (goal_store, session_key) = {
                    let s = lock_recover(&state);
                    (s.goal_store.clone(), s.session_key.clone())
                };
                let state = state.clone();
                tokio::spawn(async move {
                    match goal_store.load(&session_key).await {
                        Ok(goal) => lock_recover(&state).goal = goal,
                        Err(err) => {
                            tracing::warn!(error = %err, "failed to reload session goal")
                        }
                    }
                });
            }
            had_events = true;
        }

        // In inline mode, flush finalized messages to the native scrollback.
        {
            let mut s = lock_recover(&state);
            inline::emit_finalized_messages(&mut s, |bytes| {
                scrollback_tx
                    .send(bytes)
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer thread gone"))
            })?;
        }

        // Poll terminal events with a timeout.
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if crossterm::event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    events::handle_key_event(&mut lock_recover(&state), key, &send_tx);
                }
                Event::Paste(text) => {
                    events::route_paste(&mut lock_recover(&state), text);
                }
                Event::Mouse(mouse) => {
                    events::handle_mouse_event(&mut lock_recover(&state), mouse);
                }
                _ => {}
            }
            had_events = true;
        }
        dirty |= had_events;

        if last_tick.elapsed() >= tick_rate {
            last_tick = tokio::time::Instant::now();
            // Expire the todo panel hide timer if all items are completed.
            let mut s = lock_recover(&state);
            if s.todo_hide_at
                .is_some_and(|t| t <= std::time::Instant::now())
            {
                s.todos.clear();
                s.todo_hide_at = None;
                dirty = true;
            }
            // Drive the status-bar spinner while a run is active. Idle ticks
            // still cost nothing: no active run means no redraw.
            if s.is_active() {
                s.spinner_frame = s.spinner_frame.wrapping_add(1);
                dirty = true;
            }
            // Expire a stale transient notice (copy feedback, ...) so the
            // status bar falls back to the connection status.
            if s.notice
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= widgets::NOTICE_TTL)
            {
                s.notice = None;
                dirty = true;
            }
            drop(s);
        }

        let (should_quit, current_mode) = {
            let s = lock_recover(&state);
            (s.quit, s.screen_mode)
        };

        if current_mode != initial_mode {
            return Ok(LoopOutcome::ModeSwitch);
        }

        if dirty {
            terminal.draw(|f| render::draw_ui(f, &mut lock_recover(&state)))?;
            dirty = false;
        }

        if should_quit {
            break;
        }
    }

    Ok(LoopOutcome::Quit)
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

/// Build the WebSocket session key used by the TUI.
///
/// The peer id is unique per TUI invocation so that separate runs do not reuse
/// each other's transcripts. Within one TUI session every message shares the
/// same key, so multi-turn context is preserved.
pub(crate) fn tui_session_key(session_id: &str) -> String {
    legion_plugin_sdk::session_key::direct_session_key("main", "dm", "tui", "default", session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
    use legion_runtime::AskUserOption;
    use ratatui::text::Line;
    use std::collections::HashSet;

    fn theme() -> crate::tui::theme::Theme {
        crate::tui::theme::Theme::default()
    }

    fn highlighter() -> &'static crate::tui::syntax::Highlighter {
        static HIGHLIGHTER: std::sync::OnceLock<crate::tui::syntax::Highlighter> =
            std::sync::OnceLock::new();
        HIGHLIGHTER.get_or_init(crate::tui::syntax::Highlighter::new)
    }

    fn composer_with(text: &str) -> crate::tui::composer::Composer {
        let mut c = crate::tui::composer::Composer::new();
        c.set_text(text);
        c
    }

    #[test]
    fn message_lines_include_prefix() {
        let theme = theme();
        let msg = ChatMessage::new(MessageRole::User, "hello");
        let rendered = widgets::message_lines(&msg, 0, &HashSet::new(), 80, &theme, highlighter());
        let text = rendered.lines[0].to_string();
        assert!(text.contains("You:"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn message_lines_adds_status_indicator() {
        let theme = theme();
        let mut msg = ChatMessage::new(MessageRole::Assistant, "hello");
        msg.state = MessageState::Loading;
        let rendered = widgets::message_lines(&msg, 0, &HashSet::new(), 80, &theme, highlighter());
        let text = rendered.lines[0].to_string();
        assert!(text.contains("Legion:"));
        assert!(text.contains("◐"));
    }

    #[test]
    fn message_lines_shows_thinking_hint_before_content() {
        let theme = theme();
        let msg = ChatMessage::new(
            MessageRole::Assistant,
            "<think>secret</think>answer".to_string(),
        );
        let rendered = widgets::message_lines(&msg, 0, &HashSet::new(), 80, &theme, highlighter());
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
        let theme = theme();
        let msg = ChatMessage::new(
            MessageRole::Assistant,
            "<think>secret</think>answer".to_string(),
        );
        let mut expanded = HashSet::new();
        expanded.insert((0, 0));
        let rendered = widgets::message_lines(&msg, 0, &expanded, 80, &theme, highlighter());
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
        let theme = theme();
        let lines = markdown::markdown_lines("**bold** and *italic*", &theme, highlighter(), 80);
        let text = lines[0].to_string();
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
    }

    #[test]
    fn markdown_lines_renders_inline_code() {
        let theme = theme();
        let lines = markdown::markdown_lines("use `cargo build`", &theme, highlighter(), 80);
        let text = lines[0].to_string();
        assert!(text.contains("cargo build"));
    }

    #[test]
    fn markdown_lines_renders_code_block() {
        let theme = theme();
        let lines = markdown::markdown_lines("```rust\nlet x = 1;\n```", &theme, highlighter(), 80);
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
        let theme = theme();
        let lines = markdown::markdown_lines("# Title\n## Subtitle", &theme, highlighter(), 80);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Title"));
        assert!(text.contains("Subtitle"));
    }

    #[test]
    fn heading_uses_theme_color() {
        let theme = theme();
        let lines = markdown::markdown_lines("# Title", &theme, highlighter(), 80);
        let text_line = lines
            .iter()
            .find(|l| l.to_string().contains("Title"))
            .expect("heading line must exist");
        let colored = text_line
            .spans
            .iter()
            .find(|s| s.content.contains("Title"))
            .expect("heading span must exist");
        assert_eq!(colored.style.fg, Some(theme.heading_color(1)));
    }

    #[test]
    fn role_backgrounds_come_from_theme() {
        let theme = theme();
        assert_eq!(
            widgets::role_background(MessageRole::System, &theme),
            theme.system_bg
        );
        assert_eq!(
            widgets::role_background(MessageRole::Question, &theme),
            theme.question_bg
        );
    }

    #[test]
    fn light_theme_highlighting_does_not_panic() {
        let light = crate::tui::theme::Theme::default_light();
        let lines = markdown::markdown_lines(
            "```rust\nlet x = 1;\nlet y = 2;\n```",
            &light,
            highlighter(),
            80,
        );
        assert!(lines.len() >= 3);
    }

    #[test]
    fn markdown_lines_renders_list() {
        let theme = theme();
        let lines = markdown::markdown_lines("- a\n- b", &theme, highlighter(), 80);
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
        let theme = theme();
        let lines = markdown::markdown_lines("> quoted", &theme, highlighter(), 80);
        let text = lines[0].to_string();
        assert!(text.contains("quoted"));
        assert!(text.contains("│"));
    }

    #[test]
    fn markdown_lines_renders_horizontal_rule() {
        let theme = theme();
        let lines = markdown::markdown_lines("---", &theme, highlighter(), 80);
        let text = lines[0].to_string();
        assert!(text.contains("──"));
    }

    #[test]
    fn markdown_table_renders_aligned_columns() {
        let theme = theme();
        let lines = markdown::markdown_lines(
            "| Name | Age |\n| --- | --- |\n| Ann | 3 |\n| Bob | 42 |",
            &theme,
            highlighter(),
            80,
        );
        let rendered: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let joined = rendered.join("\n");
        assert!(joined.contains("Name"), "header cell missing: {joined}");
        assert!(joined.contains("│"), "column separator missing: {joined}");
        // Columns align: "Age" starts at the same offset in header and body rows.
        let header_row = rendered
            .iter()
            .find(|l| l.contains("Name"))
            .expect("header row");
        let body_row = rendered
            .iter()
            .find(|l| l.contains("Bob"))
            .expect("body row");
        assert_eq!(
            header_row.find("Age"),
            body_row.find("42"),
            "second column must align"
        );
    }

    #[test]
    fn code_block_lines_have_uniform_width_and_background() {
        use crate::tui::input::visible_width;
        let theme = theme();
        let lines = markdown::markdown_lines(
            "```rust\nlet x = 1;\nlet much_longer_variable = compute();\n```",
            &theme,
            highlighter(),
            80,
        );
        assert!(lines.len() >= 3, "border + 2 code lines expected");
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| visible_width(&l.to_string()))
            .collect();
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "all code block lines must share one width, got {widths:?}"
        );
        // Every line carries the code background on a padding span, so the block
        // has no ragged right edge.
        for line in &lines {
            let last = line.spans.last().expect("line has spans");
            assert_eq!(last.style.bg, Some(theme.code_bg));
        }
    }

    #[test]
    fn render_tool_card_uses_state_color() {
        let theme = theme();
        let done_lines = tool_card::render_tool_card("[tool:done] read_file", &theme);
        let error_lines = tool_card::render_tool_card("[tool:error] read_file", &theme);
        let start_lines = tool_card::render_tool_card("[tool:start] read_file", &theme);
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
        let theme = theme();
        let msg = ChatMessage {
            role: MessageRole::Assistant,
            content: "**bold**".to_string(),
            state: MessageState::Streaming,
        };
        let rendered = widgets::message_lines(&msg, 0, &HashSet::new(), 80, &theme, highlighter());
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
        let rendered = widgets::message_lines(&done, 0, &HashSet::new(), 80, &theme, highlighter());
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
        let theme = theme();
        let stdout = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = json!({ "exit_code": 0, "stdout": stdout, "stderr": "" }).to_string();
        let content = tool_card::tool_card_json("done", "exec", None, Some(&result));
        let lines = tool_card::render_tool_card(&content, &theme);
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
        let theme = theme();
        let args = "x".repeat(1000);
        let content = tool_card::tool_card_json("start", "write", Some(&args), None);
        let text: String = tool_card::render_tool_card(&content, &theme)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains('…'));
        assert!(text.len() < 800);
    }

    #[test]
    fn wrap_and_remap_shifts_hints_by_wrapped_lines() {
        let rendered = state::RenderedMessage {
            // First line wraps to 3 lines at width 10; the hint line fits.
            lines: vec![Line::from("a".repeat(25)), Line::from("[think]")],
            think_hints: vec![state::ThinkHint {
                block_index: 0,
                start_line: 1,
                line_count: 1,
            }],
        };
        let (lines, hints) = render::wrap_and_remap(rendered, 10);
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

        assert!(!state.pending_request);
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[1].role, MessageRole::System);
        assert!(state.messages[1].content.contains("HTTP 422: nope"));
        assert_eq!(state.messages[1].state, MessageState::Error);
    }

    #[test]
    fn handle_ws_event_tool_start_marks_assistant_done_and_adds_tool() {
        let theme = theme();
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].state, MessageState::Done);
        assert_eq!(state.messages[1].role, MessageRole::Tool);
        assert_eq!(state.messages[1].state, MessageState::Loading);
        let text: String = tool_card::render_tool_card(&state.messages[1].content, &theme)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("exec"));
        assert!(text.contains("running"));
    }

    #[test]
    fn handle_ws_event_tool_end_updates_card_with_result() {
        let theme = theme();
        let mut state = AppState::default();
        state.messages.push(ChatMessage {
            role: MessageRole::Tool,
            content: tool_card::tool_card_json("start", "exec", Some(r#"{"command":"ls"}"#), None),
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].state, MessageState::Done);
        let text: String = tool_card::render_tool_card(&state.messages[0].content, &theme)
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
            content: tool_card::tool_card_json("done", "exec", None, Some("ok")),
            state: MessageState::Done,
        });

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "assistant", "delta": "Here is the result." }
        });
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

        assert!(
            state.pending_approval.is_none(),
            "a prompt left pending at run end (gate timeout) must be cleared"
        );
    }

    #[test]
    fn enter_during_active_run_queues_message() {
        let mut state = AppState {
            pending_request: true, // run in flight
            ..AppState::default()
        };
        state.composer.set_text("follow-up question");
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(
            &mut state,
            event::KeyEvent::new(event::KeyCode::Enter, event::KeyModifiers::NONE),
            &tx,
        );
        assert!(
            rx.try_recv().is_err(),
            "queued message must not be sent yet"
        );
        assert_eq!(state.queued_messages.len(), 1);
        assert!(
            !state
                .messages()
                .iter()
                .any(|m| m.content.contains("follow-up")),
            "queued message must not appear in chat before it is sent"
        );
        assert_eq!(state.composer.join(), "");
    }

    #[test]
    fn lifecycle_end_drains_queue() {
        let mut state = AppState {
            pending_request: true,
            ..AppState::default()
        };
        state
            .queued_messages
            .push_back(("next question".to_string(), true));
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        let end = serde_json::json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "lifecycle", "phase": "end" }
        });
        events::handle_ws_event(&mut state, end, &tx);
        assert!(state.queued_messages.is_empty());
        assert_eq!(
            rx.try_recv().expect("queued message must be sent"),
            state::OutboundControl::Message("next question".to_string())
        );
        assert!(
            state.pending_request,
            "the drained message starts a new run"
        );
        let last = state.messages().last().expect("user message in chat");
        assert_eq!(last.content, "next question");
        assert_eq!(last.role, MessageRole::User);
    }

    #[test]
    fn handle_key_event_y_approves_pending_tool() {
        let mut state = AppState {
            pending_approval: Some(("prompt-0".to_string(), "exec".to_string())),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('y')), &tx);

        assert_eq!(
            rx.try_recv().expect("approval answer must be sent"),
            state::OutboundControl::ResolveApproval {
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
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('n')), &tx);

        assert_eq!(
            rx.try_recv().expect("approval answer must be sent"),
            state::OutboundControl::ResolveApproval {
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
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Esc), &tx);

        assert_eq!(
            rx.try_recv().expect("approval answer must be sent"),
            state::OutboundControl::ResolveApproval {
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
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();

        // Neither text input nor an accidental Enter may leak through while
        // the approval prompt is modal.
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('a')), &tx);
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        assert!(state.composer.is_empty());
        assert!(
            state.pending_approval.is_some(),
            "prompt stays pending until y/n/Esc"
        );
        assert!(rx.try_recv().is_err(), "no command may be sent");
    }

    #[test]
    fn handle_key_event_sends_user_message_and_marks_pending() {
        let mut state = AppState {
            composer: composer_with("hi"),
            ..AppState::default()
        };
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        // Only the user message is added immediately; the assistant placeholder
        // is created lazily when the first token streams in.
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::User);
        assert_eq!(state.messages[0].content, "hi");
        assert!(state.pending_request);
        assert!(state.force_scroll_bottom);
        assert!(state.composer.is_empty());
    }

    #[test]
    fn handle_key_event_slash_help_runs_locally_without_sending() {
        let mut state = AppState {
            composer: composer_with("/help"),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        // A User echo plus the System help listing, nothing else.
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].role, MessageRole::User);
        assert_eq!(state.messages[0].content, "/help");
        assert_eq!(state.messages[1].role, MessageRole::System);
        assert!(state.messages[1].content.contains("/clear"));
        assert!(!state.pending_request, "slash commands never start a turn");
        assert!(state.composer.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "slash commands must not reach the driver"
        );
    }

    #[test]
    fn handle_key_event_slash_quit_sets_quit_flag() {
        let mut state = AppState {
            composer: composer_with("/quit"),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert!(state.quit);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handle_key_event_slash_clear_empties_history() {
        let mut state = AppState {
            composer: composer_with("/clear"),
            ..AppState::default()
        };
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, "old"));
        state.render_cache.push(None);
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
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
            composer: composer_with("/tmp/foo"),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert_eq!(
            rx.try_recv()
                .expect("path-like input is sent as a normal message"),
            state::OutboundControl::Message("/tmp/foo".to_string())
        );
        assert!(state.pending_request);
        assert_eq!(state.messages[0].role, MessageRole::User);
    }

    #[test]
    fn handle_key_event_tab_completes_selected_slash_command() {
        let mut state = AppState {
            composer: composer_with("/he"),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Tab), &tx);
        assert_eq!(state.composer.join(), "/help ");
        assert_eq!(state.composer.cursor(), (0, 6));
        assert_eq!(state.slash_selected, 0);
        assert!(rx.try_recv().is_err(), "completion does not send anything");
    }

    #[test]
    fn handle_key_event_arrows_navigate_slash_menu_without_moving_cursor() {
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        let mut state = AppState {
            composer: composer_with("/"),
            input_area_width: 80,
            ..AppState::default()
        };
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.slash_selected, 1);
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.slash_selected, 2);
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.slash_selected, 1);
        assert_eq!(
            state.composer.cursor(),
            (0, 1),
            "menu navigation must not move the cursor"
        );

        // With a single suggestion, a bare Down would otherwise move the
        // cursor to the end of the input line.
        let mut state = AppState {
            composer: composer_with("/help"),
            input_area_width: 80,
            ..AppState::default()
        };
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.slash_selected, 0);
        assert_eq!(
            state.composer.cursor(),
            (0, 5),
            "menu navigation must not move the cursor"
        );
    }

    #[test]
    fn commit_and_clear_input_records_history() {
        let mut state = AppState {
            composer: composer_with("hello"),
            ..AppState::default()
        };
        input::commit_and_clear_input(&mut state, "hello");
        assert_eq!(state.input_history, vec!["hello".to_string()]);
        assert!(state.composer.is_empty());
        assert!(state.draft_input.is_none());
        assert!(state.history_index.is_none());
    }

    #[test]
    fn handle_key_event_up_down_recalls_input_history() {
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        let mut state = AppState {
            input_history: vec!["first".to_string(), "second".to_string()],
            input_area_width: 80,
            ..AppState::default()
        };

        // ↑ recalls the most recent entry.
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.composer.join(), "second");
        assert_eq!(state.composer.cursor(), (0, 6));
        assert_eq!(state.history_index, Some(1));
        assert_eq!(state.draft_input, Some(String::new()));

        // ↑ again moves to older entry.
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.composer.join(), "first");
        assert_eq!(state.history_index, Some(0));

        // ↓ moves forward through history.
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.composer.join(), "second");
        assert_eq!(state.history_index, Some(1));
    }

    #[test]
    fn handle_key_event_down_restores_draft() {
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        let mut state = AppState {
            composer: composer_with("draft"),
            input_history: vec!["previous".to_string()],
            input_area_width: 80,
            ..AppState::default()
        };

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Up), &tx);
        assert_eq!(state.composer.join(), "previous");
        assert_eq!(state.draft_input, Some("draft".to_string()));

        // ↓ past the newest entry restores the saved draft.
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(state.composer.join(), "draft");
        assert_eq!(state.composer.cursor(), (0, 5));
        assert!(state.history_index.is_none());
        assert!(state.draft_input.is_none());
    }

    #[test]
    fn handle_key_event_shift_up_down_moves_cursor_between_lines() {
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        let mut state = AppState {
            composer: composer_with("abcd\nefgh"),
            input_area_width: 80,
            ..AppState::default()
        };
        state.composer.move_cursor_top();
        state.composer.input(event::KeyEvent::from(KeyCode::Home));

        fn shift_key(code: KeyCode) -> event::KeyEvent {
            event::KeyEvent {
                code,
                modifiers: KeyModifiers::SHIFT,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::empty(),
            }
        }

        events::handle_key_event(&mut state, shift_key(KeyCode::Down), &tx);
        assert_eq!(
            state.composer.cursor(),
            (1, 0),
            "Shift+Down should move to next line"
        );
        events::handle_key_event(&mut state, shift_key(KeyCode::Up), &tx);
        assert_eq!(
            state.composer.cursor(),
            (0, 0),
            "Shift+Up should move to previous line"
        );
    }

    #[test]
    fn navigate_input_history_no_ops_when_empty() {
        let mut state = AppState::default();
        input::navigate_input_history(&mut state, true);
        assert!(state.composer.is_empty());
        assert!(state.history_index.is_none());
    }

    #[test]
    fn handle_key_event_slash_command_with_args_executes_directly() {
        let mut state = AppState {
            composer: composer_with("/help me"),
            ..AppState::default()
        };
        assert!(
            state.slash_suggestions().is_empty(),
            "a space after the command name closes the completion menu"
        );
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
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
            composer: composer_with("!echo hi"),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert_eq!(
            rx.try_recv().expect("shell command must be sent"),
            state::OutboundControl::ShellCommand("echo hi".to_string())
        );
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::User);
        assert_eq!(state.messages[0].content, "!echo hi");
        assert!(state.composer.is_empty());
        assert!(!state.pending_request);
    }

    #[test]
    fn handle_key_event_bang_only_reports_empty_shell_command() {
        let mut state = AppState {
            composer: composer_with("!"),
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].role, MessageRole::System);
        assert!(state.messages[0].content.contains("empty"));
        assert!(rx.try_recv().is_err(), "empty shell command is not sent");
        assert!(state.composer.is_empty());
    }

    #[test]
    fn input_title_reflects_bang_prefix() {
        let state = AppState {
            composer: composer_with("!pwd"),
            ..AppState::default()
        };
        // The title is derived from input content, not a separate flag.
        assert!(state.composer.join().starts_with('!'));
    }

    #[test]
    fn composer_cursor_moves_left_right() {
        let mut composer = composer_with("abc");
        composer.input(event::KeyEvent::from(KeyCode::End));
        composer.input(event::KeyEvent::from(KeyCode::Left));
        assert_eq!(composer.cursor(), (0, 2));
        composer.input(event::KeyEvent::from(KeyCode::Right));
        assert_eq!(composer.cursor(), (0, 3));
    }

    #[test]
    fn composer_insert_and_delete_at_cursor() {
        let mut composer = composer_with("ac");
        composer.input(event::KeyEvent::from(KeyCode::Home));
        composer.input(event::KeyEvent::from(KeyCode::Right));
        composer.input(event::KeyEvent::from(KeyCode::Char('b')));
        assert_eq!(composer.join(), "abc");
        assert_eq!(composer.cursor(), (0, 2));
        composer.input(event::KeyEvent::from(KeyCode::Backspace));
        assert_eq!(composer.join(), "ac");
        assert_eq!(composer.cursor(), (0, 1));
    }

    #[test]
    fn composer_home_end() {
        let mut composer = composer_with("hello world");
        composer.input(event::KeyEvent::from(KeyCode::Home));
        assert_eq!(composer.cursor(), (0, 0));
        composer.input(event::KeyEvent::from(KeyCode::End));
        assert_eq!(composer.cursor(), (0, 11));
    }

    #[test]
    fn composer_cursor_moves_between_lines() {
        let mut composer = composer_with("abcd\nefgh");
        composer.move_cursor_top();
        composer.input(event::KeyEvent::from(KeyCode::Home));
        assert_eq!(composer.cursor(), (0, 0));
        composer.move_cursor_down();
        assert_eq!(composer.cursor(), (1, 0));
        composer.move_cursor_end();
        assert_eq!(composer.cursor(), (1, 4));
        composer.move_cursor_up();
        assert_eq!(composer.cursor(), (0, 4));
    }

    #[test]
    fn parse_segments_splits_text_and_think_blocks() {
        let segments = widgets::parse_message_segments("hello <think>reasoning</think> world");
        assert_eq!(
            segments,
            vec![
                widgets::MessageSegment::Text("hello "),
                widgets::MessageSegment::Think {
                    index: 0,
                    text: "reasoning"
                },
                widgets::MessageSegment::Text(" world"),
            ]
        );
    }

    #[test]
    fn parse_segments_treats_unclosed_think_as_reasoning() {
        let segments = widgets::parse_message_segments("<think>still thinking");
        assert_eq!(
            segments,
            vec![widgets::MessageSegment::Think {
                index: 0,
                text: "still thinking"
            }]
        );
    }

    #[test]
    fn parse_segments_ignores_empty_think_block() {
        let segments = widgets::parse_message_segments("before<think></think>after");
        assert_eq!(
            segments,
            vec![
                widgets::MessageSegment::Text("before"),
                widgets::MessageSegment::Text("after"),
            ]
        );
    }

    #[test]
    fn parse_segments_handles_leading_think_block() {
        let segments = widgets::parse_message_segments("<think>reason</think>answer");
        assert_eq!(
            segments,
            vec![
                widgets::MessageSegment::Think {
                    index: 0,
                    text: "reason"
                },
                widgets::MessageSegment::Text("answer"),
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
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageDown), &tx);
        assert_eq!(state.scroll, 5);

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageDown), &tx);
        assert_eq!(state.scroll, 10);

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageUp), &tx);
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
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(
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

        events::handle_key_event(
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
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('t')), &tx);
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Char('q')), &tx);

        assert_eq!(state.composer.join(), "tq");
        assert!(!state.quit);
        assert!(state.expanded_thinks.is_empty());
    }

    #[test]
    fn handle_key_event_ctrl_q_quits() {
        let mut state = AppState::default();
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, ctrl_key(KeyCode::Char('q')), &tx);

        assert!(state.quit);
    }

    #[test]
    fn handle_key_event_ctrl_t_toggles_thinking() {
        let mut state = AppState::default();
        state.messages.push(ChatMessage::new(
            MessageRole::Assistant,
            "<think>secret</think>answer".to_string(),
        ));
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, ctrl_key(KeyCode::Char('t')), &tx);
        assert!(state.expanded_thinks.contains(&(0, 0)));
        assert!(state.composer.is_empty());

        events::handle_key_event(&mut state, ctrl_key(KeyCode::Char('t')), &tx);
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

        events::handle_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert_eq!(state.scroll, 13);

        events::handle_mouse_event(
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
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::PageUp), &tx);
        assert_eq!(state.scroll, 0);

        events::handle_mouse_event(
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
        input::apply_scroll(&mut state, 20);
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
        input::apply_scroll(&mut state, 25);
        assert_eq!(state.scroll, 5);
    }

    #[test]
    fn apply_scroll_clamps_when_content_shrinks() {
        let mut state = AppState {
            scroll: 15,
            max_scroll: 20,
            ..AppState::default()
        };
        input::apply_scroll(&mut state, 10);
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
        input::apply_scroll(&mut state, 30);
        assert_eq!(state.scroll, 30);
        assert!(!state.force_scroll_bottom);
    }

    #[test]
    fn handle_key_event_enter_forces_scroll_bottom() {
        let mut state = AppState {
            composer: composer_with("hi"),
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        assert!(state.force_scroll_bottom);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn handle_paste_inserts_small_text() {
        let mut state = AppState::default();
        input::handle_paste(&mut state, "hello".to_string());
        assert_eq!(state.composer.join(), "hello");
        assert!(state.paste_store.is_empty());
    }

    #[test]
    fn handle_paste_inserts_multiline_text_under_threshold() {
        let mut state = AppState::default();
        let text = "line1\nline2\nline3".to_string();
        input::handle_paste(&mut state, text.clone());
        assert_eq!(state.composer.join(), text);
        assert!(state.paste_store.is_empty());
    }

    #[test]
    fn handle_paste_collapses_long_text() {
        let mut state = AppState::default();
        let text = "x".repeat(state::PASTE_CHAR_THRESHOLD + 1);
        input::handle_paste(&mut state, text.clone());
        let joined = state.composer.join();
        assert!(!joined.contains(&text));
        assert!(joined.contains("Pasted text"));
        assert!(joined.contains(&format!("{} chars", text.len())));
        assert_eq!(state.paste_store.len(), 1);
        assert!(state.paste_store.values().any(|v| v == &text));
    }

    #[test]
    fn handle_paste_collapses_many_lines() {
        let mut state = AppState::default();
        let text = (0..state::PASTE_LINE_THRESHOLD + 1)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        input::handle_paste(&mut state, text.clone());
        let joined = state.composer.join();
        assert!(joined.contains("Pasted text"));
        assert!(joined.contains(&format!("{} lines", state::PASTE_LINE_THRESHOLD + 1)));
        assert_eq!(state.paste_store.len(), 1);
    }

    #[test]
    fn expand_paste_placeholders_restores_original_text() {
        let mut store = std::collections::HashMap::new();
        store.insert(
            "[...Pasted text #0: 3 lines, 12 chars...]".to_string(),
            "a\nb\nc".to_string(),
        );
        let input = "before [...Pasted text #0: 3 lines, 12 chars...] after".to_string();
        let expanded = input::expand_paste_placeholders(&input, &store);
        assert_eq!(expanded, "before a\nb\nc after");
    }

    #[test]
    fn enter_sends_expanded_paste_content() {
        let mut state = AppState::default();
        let content = "x".repeat(state::PASTE_CHAR_THRESHOLD + 1);
        input::handle_paste(&mut state, content.clone());
        assert_eq!(state.paste_store.len(), 1);

        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        let sent = rx.try_recv().expect("expected a message to be sent");
        assert_eq!(sent, state::OutboundControl::Message(content));
        assert!(state.composer.is_empty());
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
        let msgs = events::history_messages_from_payload(&resp);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[1].content, "hello");
    }

    #[test]
    fn history_messages_from_payload_tolerates_missing_payload() {
        assert!(events::history_messages_from_payload(&json!({})).is_empty());
        assert!(events::history_messages_from_payload(&json!({"payload": {}})).is_empty());
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
            composer: composer_with("hello"),
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render::draw_ui(f, &mut state)).unwrap();

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
            composer: composer_with("hello world"),
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        // Very small terminal: chat area height = 5 - 2 borders = 3 visible lines.
        // The new role bar widens the prefix, so the message may wrap; we just
        // verify every part of it is still visible.
        let backend = TestBackend::new(20, 11);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render::draw_ui(f, &mut state)).unwrap();

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
            composer: composer_with("this is a long user message"),
            ..AppState::default()
        };
        let (tx, _) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        // Width 20, height 12 => chat area height 5 => 3 visible lines.
        // The long user message plus prefix wraps, so correct scroll math is
        // required to keep the tail visible.
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render::draw_ui(f, &mut state)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            text.contains("You:") && text.contains("this is a long") && text.contains("message"),
            "last wrapped user message should be visible; buffer:\n{}",
            text
        );
    }

    fn sample_pending_question(multi_select: bool) -> state::PendingQuestion {
        state::PendingQuestion {
            prompt_id: "prompt-1".to_string(),
            questions: vec![
                legion_runtime::AskUserQuestion {
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
                legion_runtime::AskUserQuestion {
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
            selected_labels: std::collections::HashMap::new(),
            focused: 0,
            message_index: 0,
        }
    }

    #[test]
    fn question_renders_question_tabs_with_vertical_options() {
        let pq = sample_pending_question(false);
        let msg = question::format_question_message(&pq);
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();

        // Tab count = 2 questions + Submit = 3. current starts at 0 (Color).
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Left), &tx);
        assert_eq!(
            state.pending_question.as_ref().unwrap().current,
            2,
            "left from first tab should wrap to Submit"
        );

        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
        assert_eq!(
            state.pending_question.as_ref().unwrap().current,
            0,
            "right from Submit should wrap to first question"
        );

        // Up/Down navigate options within the current question.
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        assert_eq!(
            state.pending_question.as_ref().unwrap().focused,
            1,
            "down should move to Blue option"
        );

        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
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
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();

        // Select Red on the Color question.
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
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
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);
        {
            let pq = state.pending_question.as_ref().unwrap();
            assert!(
                pq.is_selected("Which size?", "Large"),
                "Large should be selected"
            );
        }

        // Move to Submit tab and confirm.
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Right), &tx);
        assert!(state.pending_question.as_ref().unwrap().is_submit_tab());
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Enter), &tx);

        assert!(
            state.pending_question.is_none(),
            "prompt should be resolved after submitting"
        );
        let sent = rx.try_recv().expect("answer should be sent");
        match sent {
            state::OutboundControl::ResolveQuestion { output, .. } => {
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();

        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Char(' ')), &tx);
        assert!(
            state
                .pending_question
                .as_ref()
                .unwrap()
                .is_selected("Which color?", "Red")
        );

        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Down), &tx);
        events::handle_question_key(&mut state, event::KeyEvent::from(KeyCode::Char(' ')), &tx);
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
        let theme = theme();
        let state = AppState {
            todos: vec![
                legion_runtime::TodoItem {
                    id: "1".into(),
                    content: "Plan migration".into(),
                    status: legion_runtime::TodoStatus::InProgress,
                    active_form: "Planning migration".into(),
                },
                legion_runtime::TodoItem {
                    id: "2".into(),
                    content: "Run tests".into(),
                    status: legion_runtime::TodoStatus::Completed,
                    active_form: String::new(),
                },
            ],
            todo_max_display: 6,
            ..AppState::default()
        };

        let lines = widgets::render_todo_panel(&state, 40, &theme);
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
            todos: vec![legion_runtime::TodoItem {
                id: "1".into(),
                content: "Old".into(),
                status: legion_runtime::TodoStatus::InProgress,
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
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

        assert_eq!(state.todos.len(), 2);
        assert_eq!(state.todos[0].content, "Updated");
        assert_eq!(state.todos[1].status, legion_runtime::TodoStatus::Completed);
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
        events::handle_ws_event(&mut state, all_done, &tx);
        assert!(state.todo_hide_at.is_some(), "hide scheduled when all done");
    }

    #[test]
    fn todo_update_empty_list_clears_todos() {
        let mut state = AppState {
            todos: vec![legion_runtime::TodoItem {
                id: "1".into(),
                content: "Stale".into(),
                status: legion_runtime::TodoStatus::InProgress,
                active_form: String::new(),
            }],
            ..AppState::default()
        };

        let event = json!({
            "type": "event",
            "event": "agent",
            "payload": { "stream": "todo_update", "items": [] }
        });
        let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_ws_event(&mut state, event, &tx);

        assert!(state.todos.is_empty());
        assert!(state.todo_hide_at.is_none());
    }

    #[test]
    fn truncate_to_width_respects_display_width() {
        assert_eq!(widgets::truncate_to_width("hello", 5), "hello");
        assert_eq!(widgets::truncate_to_width("hello world", 5), "hell…");
        // CJK characters are roughly 2 display columns each.
        assert_eq!(widgets::truncate_to_width("中文", 3), "中…");
        assert_eq!(widgets::truncate_to_width("中文", 4), "中文");
    }

    #[test]
    fn screen_mode_name_roundtrip() {
        assert_eq!(
            ScreenMode::from_name("fullscreen"),
            Some(ScreenMode::Fullscreen)
        );
        assert_eq!(ScreenMode::from_name("inline"), Some(ScreenMode::Inline));
        assert_eq!(ScreenMode::from_name("bogus"), None);
        assert_eq!(ScreenMode::Fullscreen.name(), "fullscreen");
        assert_eq!(ScreenMode::Inline.name(), "inline");
    }

    #[test]
    fn theme_command_does_not_persist_without_config_path() {
        let mut state = AppState::default(); // config_path is None in tests
        let result = crate::slash_commands::dispatch(&mut state, "/theme light");
        assert!(matches!(
            result,
            crate::slash_commands::CommandResult::Handled
        ));
        assert_eq!(state.theme, crate::tui::theme::Theme::default_light());
        assert_eq!(state.theme_name, "light");
        let last = state.messages().last().expect("feedback message");
        assert!(last.content.contains("theme set to light"));
        assert!(!last.content.contains("saved"));
    }

    #[test]
    fn esc_during_active_run_sends_cancel() {
        let mut state = AppState {
            pending_request: true, // run in flight, no first token yet
            ..AppState::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(
            &mut state,
            event::KeyEvent::new(event::KeyCode::Esc, event::KeyModifiers::NONE),
            &tx,
        );
        assert_eq!(
            rx.try_recv().expect("cancel must be sent"),
            state::OutboundControl::Cancel
        );
        let last = state.messages().last().expect("feedback message");
        assert!(last.content.contains("cancelling"));
    }

    #[test]
    fn esc_when_idle_sends_nothing() {
        let mut state = AppState::default();
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(
            &mut state,
            event::KeyEvent::new(event::KeyCode::Esc, event::KeyModifiers::NONE),
            &tx,
        );
        assert!(rx.try_recv().is_err(), "idle Esc must not send anything");
    }

    #[test]
    fn alt_enter_inserts_newline_instead_of_sending() {
        let mut state = AppState::default();
        state.composer.set_text("first line");
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(
            &mut state,
            event::KeyEvent::new(event::KeyCode::Enter, event::KeyModifiers::ALT),
            &tx,
        );
        assert!(rx.try_recv().is_err(), "Alt+Enter must not send");
        assert_eq!(state.composer.join(), "first line\n");
    }

    #[test]
    fn plain_enter_still_sends() {
        let mut state = AppState::default();
        state.composer.set_text("hello");
        let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
        events::handle_key_event(
            &mut state,
            event::KeyEvent::new(event::KeyCode::Enter, event::KeyModifiers::NONE),
            &tx,
        );
        assert_eq!(
            rx.try_recv().expect("plain Enter must send"),
            state::OutboundControl::Message("hello".to_string())
        );
    }

    #[test]
    fn paste_during_approval_is_dropped() {
        let mut state = AppState {
            pending_approval: Some(("p1".to_string(), "exec".to_string())),
            ..Default::default()
        };
        events::route_paste(&mut state, "rm -rf /".to_string());
        assert_eq!(state.composer.join(), "");
    }

    #[test]
    fn paste_during_question_is_dropped() {
        let mut state = AppState::default();
        state.pending_question = Some(crate::tui::state::PendingQuestion {
            prompt_id: "q1".to_string(),
            questions: vec![],
            current: 0,
            selected_labels: std::collections::HashMap::new(),
            focused: 0,
            message_index: 0,
        });
        events::route_paste(&mut state, "answer text".to_string());
        assert_eq!(state.composer.join(), "");
    }

    #[test]
    fn paste_into_history_search_extends_query_single_line() {
        let mut state = AppState {
            history_search: Some(crate::tui::history_search::HistorySearch::new()),
            ..Default::default()
        };
        events::route_paste(&mut state, "cargo\nbuild".to_string());
        let hs = state.history_search.as_ref().expect("search still open");
        assert_eq!(hs.query, "cargo build");
        assert_eq!(state.composer.join(), "");
    }

    #[test]
    fn ctrl_char_does_not_leak_into_history_search_query() {
        let mut state = AppState {
            history_search: Some(crate::tui::history_search::HistorySearch::new()),
            ..Default::default()
        };
        events::handle_key_event(
            &mut state,
            event::KeyEvent::new(event::KeyCode::Char('w'), event::KeyModifiers::CONTROL),
            &mpsc::unbounded_channel::<state::OutboundControl>().0,
        );
        let hs = state.history_search.as_ref().expect("search still open");
        assert_eq!(hs.query, "");
    }

    #[test]
    fn status_bar_shows_spinner_and_cancel_hint_while_active() {
        let theme = theme();
        let mut state = AppState {
            pending_request: true,
            ..Default::default()
        };
        state.spinner_frame = 0;
        let lines = widgets::status_bar_lines(&state, &theme, 2);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains(widgets::SPINNER[0]));
        assert!(text.contains("esc to cancel"));
    }

    #[test]
    fn chat_block_shows_scroll_and_queue_indicators() {
        let mut state = AppState::default();
        for i in 0..50 {
            state.push_message(MessageRole::Assistant, format!("line {i}"));
        }
        // Simulate "user scrolled up": a nonzero previous max_scroll keeps
        // `apply_scroll` from snapping back to the bottom on the next draw.
        state.max_scroll = 1;
        state.scroll = 0;
        state.queued_messages.push_back(("later".to_string(), true));
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render::draw_ui(f, &mut state))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("↓ more"),
            "scroll indicator missing: {content}"
        );
        assert!(
            content.contains("1 queued"),
            "queue indicator missing: {content}"
        );
    }

    #[test]
    fn copy_sets_notice_shown_in_status_bar() {
        let theme = theme();
        let state = AppState {
            notice: Some(("copied 42 chars".to_string(), std::time::Instant::now())),
            ..Default::default()
        };
        let lines = widgets::status_bar_lines(&state, &theme, 2);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("copied 42 chars"));
    }

    #[test]
    fn expired_notice_falls_back_to_status() {
        let theme = theme();
        let state = AppState {
            status: "local".to_string(),
            notice: Some((
                "copied 42 chars".to_string(),
                std::time::Instant::now() - std::time::Duration::from_secs(10),
            )),
            ..Default::default()
        };
        let lines = widgets::status_bar_lines(&state, &theme, 2);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("copied"));
        assert!(text.contains("local"));
    }

    #[test]
    fn status_command_reports_theme_and_viewport() {
        let mut state = AppState {
            status: "local".to_string(),
            session_peer: "peer123".to_string(),
            theme_name: "light".to_string(),
            ..AppState::default()
        };
        let result = crate::slash_commands::dispatch(&mut state, "/status");
        assert!(matches!(
            result,
            crate::slash_commands::CommandResult::Handled
        ));
        let last = state.messages().last().expect("status message");
        assert!(last.content.contains("theme: light"));
        assert!(last.content.contains("viewport: fullscreen"));
    }

    #[test]
    fn visible_width_ignores_osc8_sequences() {
        use crate::tui::input::visible_width;
        let link = crate::tui::links::osc8_link("https://example.com/some/long/url", "example");
        assert_eq!(visible_width(&link), 7);
    }

    #[test]
    fn wrap_does_not_split_or_miscount_osc8_links() {
        use crate::tui::input::visible_width;
        let link = crate::tui::links::osc8_link("https://example.com", "a]very[long~display~text");
        let line = Line::from(ratatui::text::Span::raw(link.clone()));
        let wrapped = render::wrap_line_to_width(line, 10);
        assert!(wrapped.len() > 1, "long display text must wrap");
        for piece in &wrapped {
            assert!(visible_width(&piece.to_string()) <= 10);
        }
        // No escape sequence is torn apart: the concatenation of all pieces
        // reproduces the original string exactly.
        let reassembled: String = wrapped
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_eq!(reassembled, link);
    }
}
