//! Slash commands for the interactive TUI (`/help`, `/clear`, `/goal`, `/loop`, ...).
//!
//! Commands are handled locally inside the TUI — they never reach the agent
//! runtime, so behavior is identical in gateway and embedded mode (the
//! interception happens in `handle_key_event` before the driver).
//!
//! Three command kinds:
//! - **Local** (`/help`, `/clear`, `/status`, `/quit`, `/skills`, `/goal`, `/theme`):
//!   executed entirely in-process; the agent never sees them.
//! - **Prompt** (`/skills:<name>`): injects `body` as a system message, then
//!   sends the user's args to the agent as a normal turn.
//! - **ScheduleLoop** (`/loop`): parsed locally, then scheduled as a recurring
//!   cron job through the gateway (requires gateway mode).

use crate::goal;
use crate::loop_cmd;
use crate::tui::ScreenMode;
use crate::tui::theme::Theme;
use crate::tui::{AppState, MessageRole};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// What a slash command does when dispatched.
#[derive(Clone)]
pub enum CommandKind {
    /// Executed locally in the TUI; the agent never sees it. The handler
    /// returns the dispatch result so commands like `/loop` can request
    /// follow-on async work.
    Local {
        run: fn(&mut AppState, &str) -> CommandResult,
    },
    /// Prompt-type: inject `body` as a system message, then send the user's
    /// args to the agent as a normal turn.
    Prompt { body: String },
}

/// A slash command (builtin or skill-backed).
#[derive(Clone)]
pub struct SlashCommand {
    /// Full command name without the leading '/', e.g. `help` or
    /// `skills:clarify`.
    pub name: String,
    /// Alternative names (builtins only; skills have none).
    pub aliases: Vec<String>,
    /// One-line description shown in the completion menu and `/help`.
    pub description: String,
    /// Argument hint such as `[input]`; empty means the command takes no
    /// arguments, so Enter executes it immediately instead of completing.
    pub arg_hint: String,
    /// Dispatch behaviour.
    pub kind: CommandKind,
}

/// The outcome of dispatching a `/...` input.
#[derive(Debug)]
pub enum CommandResult {
    /// The command was handled entirely locally (builtin). Nothing is sent
    /// to the agent.
    Handled,
    /// A prompt-type skill command was matched: the body has been injected as
    /// a system message, and `message` should be sent to the agent as a user
    /// turn.
    SendToAgent { message: String },
    /// `/loop` parsed successfully: schedule the prompt as a recurring cron
    /// job and also run it now.
    ScheduleLoop { interval: String, prompt: String },
    /// The input is not a command (e.g. `/tmp/foo`) and should be sent as a
    /// normal message.
    NotACommand,
}

// ---------------------------------------------------------------------------
// Builtins
// ---------------------------------------------------------------------------

/// Construct the builtin (static) commands. These are recreated on each call
/// (the struct owns `String`s now) so skills and builtins can live in one
/// homogeneous `Vec<SlashCommand>`.
pub fn builtins() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "help".into(),
            aliases: vec![],
            description: "list available slash commands".into(),
            arg_hint: "".into(),
            kind: CommandKind::Local { run: cmd_help },
        },
        SlashCommand {
            name: "clear".into(),
            aliases: vec![],
            description: "clear the chat history (local view only)".into(),
            arg_hint: "".into(),
            kind: CommandKind::Local { run: cmd_clear },
        },
        SlashCommand {
            name: "status".into(),
            aliases: vec![],
            description: "show connection and session status".into(),
            arg_hint: "".into(),
            kind: CommandKind::Local { run: cmd_status },
        },
        SlashCommand {
            name: "skills".into(),
            aliases: vec![],
            description: "list loaded skills (use /skills:<name> to invoke)".into(),
            arg_hint: "".into(),
            kind: CommandKind::Local { run: cmd_skills },
        },
        SlashCommand {
            name: "quit".into(),
            aliases: vec!["exit".into(), "q".into()],
            description: "quit the TUI".into(),
            arg_hint: "".into(),
            kind: CommandKind::Local { run: cmd_quit },
        },
        SlashCommand {
            name: "goal".into(),
            aliases: vec![],
            description: "manage the current session goal".into(),
            arg_hint: "[start|edit|pause|resume|complete|block|clear] ...".into(),
            kind: CommandKind::Local { run: cmd_goal },
        },
        SlashCommand {
            name: "loop".into(),
            aliases: vec![],
            description: "ask the agent to schedule a recurring prompt".into(),
            arg_hint: "<when> <what>".into(),
            kind: CommandKind::Local { run: cmd_loop },
        },
        SlashCommand {
            name: "theme".into(),
            aliases: vec![],
            description: "switch the TUI color theme".into(),
            arg_hint: "<dark|light|default>".into(),
            kind: CommandKind::Local { run: cmd_theme },
        },
        SlashCommand {
            name: "mode".into(),
            aliases: vec![],
            description: "switch between fullscreen and inline viewport".into(),
            arg_hint: "<fullscreen|inline>".into(),
            kind: CommandKind::Local { run: cmd_mode },
        },
    ]
}

/// Build skill-backed commands (`/skills:<name>`) from loaded skills.
pub fn skill_commands(skills: &[legion_skills::Skill]) -> Vec<SlashCommand> {
    skills
        .iter()
        .filter(|s| s.frontmatter.user_invocable)
        .map(|s| SlashCommand {
            name: format!("skills:{}", s.frontmatter.name),
            aliases: vec![],
            description: s.frontmatter.description.clone(),
            arg_hint: "[input]".into(),
            kind: CommandKind::Prompt {
                body: s.body.clone(),
            },
        })
        .collect()
}

/// All available commands: builtins first, then skill commands.
pub fn all_commands(skills: &[legion_skills::Skill]) -> Vec<SlashCommand> {
    let mut cmds = builtins();
    cmds.extend(skill_commands(skills));
    cmds
}

// ---------------------------------------------------------------------------
// Completion / suggestions
// ---------------------------------------------------------------------------

/// Maximum number of suggestions shown in the completion menu.
const MAX_SUGGESTIONS: usize = 8;

/// Completion candidates for `query` (the text after the leading '/').
///
/// Weighted scoring without a fuzzy-match dependency:
/// exact name 100 > name prefix 80 > alias prefix 70 > name substring 50 >
/// description substring 20. Case-insensitive.
///
/// - **Empty query** (bare `/`): builtins first (help leading), then skills,
///   capped at [`MAX_SUGGESTIONS`]. This keeps the menu short — the user
///   types more to narrow down.
/// - **Non-empty query**: scored matches, capped at [`MAX_SUGGESTIONS`].
pub fn suggestions(query: &str, skills: &[legion_skills::Skill]) -> Vec<SlashCommand> {
    let commands = all_commands(skills);
    let query = query.to_lowercase();
    if query.is_empty() {
        // Builtins first (help leading), then skills; cap to keep the menu
        // manageable. The user types more to see further matches.
        let mut all = commands;
        all.sort_by(|a, b| {
            let a_builtin = !a.name.contains(':');
            let b_builtin = !b.name.contains(':');
            // Builtins before skills; within each group `help` first, then
            // alphabetical.
            b_builtin
                .cmp(&a_builtin)
                .then_with(|| (a.name != "help").cmp(&(b.name != "help")))
                .then_with(|| a.name.cmp(&b.name))
        });
        all.truncate(MAX_SUGGESTIONS);
        return all;
    }
    let mut scored: Vec<(u32, SlashCommand)> = commands
        .into_iter()
        .filter_map(|cmd| score(&cmd, &query).map(|points| (points, cmd)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.truncate(MAX_SUGGESTIONS);
    scored.into_iter().map(|(_, cmd)| cmd).collect()
}

/// Score one command against a lowercased query; `None` means no match.
fn score(cmd: &SlashCommand, query: &str) -> Option<u32> {
    let name = cmd.name.to_lowercase();
    let desc = cmd.description.to_lowercase();
    if name == query {
        Some(100)
    } else if name.starts_with(query) {
        Some(80)
    } else if cmd
        .aliases
        .iter()
        .any(|alias| alias.to_lowercase().starts_with(query))
    {
        Some(70)
    } else if name.contains(query) {
        Some(50)
    } else if desc.contains(query) {
        Some(20)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch `text` (starting with '/') as a slash command.
///
/// Path-like input such as `/tmp/x` returns [`CommandResult::NotACommand`]
/// so it degrades to a normal message — the same path guard Claude Code
/// applies.
pub fn dispatch(state: &mut AppState, text: &str) -> CommandResult {
    let body = text.strip_prefix('/').unwrap_or(text);
    let (name, args) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], body[idx..].trim_start()),
        None => (body, ""),
    };
    if name.is_empty() {
        return CommandResult::NotACommand;
    }
    let lookup = name.to_lowercase();
    let commands = all_commands(&state.loaded_skills);
    if let Some(cmd) = commands
        .iter()
        .find(|cmd| cmd.name == lookup || cmd.aliases.iter().any(|a| a == &lookup))
    {
        match &cmd.kind {
            CommandKind::Local { run } => {
                // Echo the raw input, then run locally.
                state.push_message(MessageRole::User, text);
                run(state, args)
            }
            CommandKind::Prompt { .. } => {
                // Echo the user's command and show a compact skill indicator;
                // the full skill body is available via /skills. The args are
                // still forwarded to the agent as a normal user turn.
                state.push_message(MessageRole::User, text);
                state.push_message(MessageRole::System, format!("[skill: {}]", cmd.name));
                let message = if args.is_empty() {
                    "follow the skill instructions above".to_string()
                } else {
                    args.to_string()
                };
                CommandResult::SendToAgent { message }
            }
        }
    } else if looks_like_path(name) {
        CommandResult::NotACommand
    } else {
        state.push_message(
            MessageRole::System,
            format!("unknown command '/{name}' — try /help"),
        );
        CommandResult::Handled
    }
}

/// A command name that looks like a filesystem path is probably the user
/// pasting `/tmp/...` rather than invoking a command.
fn looks_like_path(name: &str) -> bool {
    name.contains(['/', '\\']) && !name.starts_with("skills:")
        || name.starts_with('.')
        || name.starts_with('~')
}

// ---------------------------------------------------------------------------
// Builtin handlers
// ---------------------------------------------------------------------------

fn cmd_help(state: &mut AppState, _args: &str) -> CommandResult {
    let mut lines = vec!["commands:".to_string()];
    for cmd in builtins() {
        let aliases = if cmd.aliases.is_empty() {
            String::new()
        } else {
            format!(
                " (aliases: {})",
                cmd.aliases
                    .iter()
                    .map(|alias| format!("/{alias}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let arg_hint = if cmd.arg_hint.is_empty() {
            String::new()
        } else {
            format!(" {}", cmd.arg_hint)
        };
        lines.push(format!(
            "  /{}{}{} — {}",
            cmd.name, arg_hint, aliases, cmd.description
        ));
    }
    lines.push("  !<command> — run a shell command locally and show the output".to_string());

    // Skill section
    let skills = &state.loaded_skills;
    let invocable: Vec<_> = skills
        .iter()
        .filter(|s| s.frontmatter.user_invocable)
        .collect();
    if invocable.is_empty() {
        lines.push(
            "\nskills: none loaded (add SKILL.md under .agents/skills or .legion/skills)".into(),
        );
    } else {
        lines.push(format!(
            "\nskills ({} loaded, use /skills:<name> [input] to invoke):",
            invocable.len()
        ));
        for skill in &invocable {
            lines.push(format!(
                "  /skills:{} — {}",
                skill.frontmatter.name, skill.frontmatter.description
            ));
        }
    }
    state.push_message(MessageRole::System, lines.join("\n"));
    CommandResult::Handled
}

fn cmd_clear(state: &mut AppState, _args: &str) -> CommandResult {
    state.clear_messages();
    CommandResult::Handled
}

fn cmd_status(state: &mut AppState, _args: &str) -> CommandResult {
    let peer = if state.session_peer.is_empty() {
        "(unknown)".to_string()
    } else {
        state.session_peer.clone()
    };
    state.push_message(
        MessageRole::System,
        format!(
            "status: {}\nsession: {peer} (resume with `legion --session {peer}`)",
            state.status
        ),
    );
    CommandResult::Handled
}

fn cmd_skills(state: &mut AppState, _args: &str) -> CommandResult {
    let skills = &state.loaded_skills;
    let invocable: Vec<_> = skills
        .iter()
        .filter(|s| s.frontmatter.user_invocable)
        .collect();
    if invocable.is_empty() {
        state.push_message(
            MessageRole::System,
            "no skills loaded. Add SKILL.md files under .agents/skills/ or .legion/skills/ in your workspace, or under ~/.agents/skills/ or ~/.legion/skills/ globally.".to_string(),
        );
        return CommandResult::Handled;
    }
    let mut lines = vec![format!("skills ({} loaded):", invocable.len())];
    for skill in &invocable {
        let paths = if skill.frontmatter.paths.is_empty() {
            String::new()
        } else {
            format!("  [paths: {}]", skill.frontmatter.paths.join(", "))
        };
        lines.push(format!(
            "  /skills:{} — {}{}",
            skill.frontmatter.name, skill.frontmatter.description, paths
        ));
    }
    lines.push("use /skills:<name> [input] to invoke a skill".into());
    state.push_message(MessageRole::System, lines.join("\n"));
    CommandResult::Handled
}

fn cmd_quit(state: &mut AppState, _args: &str) -> CommandResult {
    state.request_quit();
    CommandResult::Handled
}

fn cmd_goal(state: &mut AppState, args: &str) -> CommandResult {
    let action = goal::parse_goal(args);
    let (new_goal, reply) = goal::apply_action(state.goal.clone(), action);
    state.goal = new_goal.clone();

    // Persist asynchronously so the TUI input thread never blocks on I/O.
    let store = state.goal_store.clone();
    let session_key = state.session_key.clone();
    if let Some(goal) = new_goal {
        tokio::spawn(async move {
            if let Err(err) = store.save(&session_key, &goal).await {
                tracing::warn!(error = %err, "failed to persist goal");
            }
        });
    } else {
        tokio::spawn(async move {
            if let Err(err) = store.remove(&session_key).await {
                tracing::warn!(error = %err, "failed to remove goal");
            }
        });
    }

    state.push_message(MessageRole::System, reply);
    CommandResult::Handled
}

fn cmd_loop(state: &mut AppState, args: &str) -> CommandResult {
    match loop_cmd::parse_loop(args) {
        Ok(req) => match loop_cmd::interval_to_cron(&req.interval) {
            Ok(cron) => {
                let human = loop_cmd::cron_human_summary(&cron);
                state.push_message(
                    MessageRole::System,
                    format!(
                        "Scheduling loop: {} ({}).\nPrompt: {}",
                        cron, human, req.prompt
                    ),
                );
                CommandResult::ScheduleLoop {
                    interval: cron,
                    prompt: req.prompt,
                }
            }
            Err(err) => {
                state.push_message(
                    MessageRole::System,
                    format!("Usage: /loop [interval] <prompt>\n{err}"),
                );
                CommandResult::Handled
            }
        },
        Err(err) => {
            state.push_message(
                MessageRole::System,
                format!("Usage: /loop [interval] <prompt>\n{err}"),
            );
            CommandResult::Handled
        }
    }
}

fn cmd_theme(state: &mut AppState, args: &str) -> CommandResult {
    let name = args.trim().to_lowercase();
    let display_name = if name.is_empty() { "default" } else { &name };
    match name.as_str() {
        "dark" => state.theme = Theme::default_dark(),
        "light" => state.theme = Theme::default_light(),
        "" | "default" => state.theme = Theme::default(),
        _ => {
            state.push_message(
                MessageRole::System,
                format!("unknown theme '/theme {name}' — try dark, light, or default"),
            );
            return CommandResult::Handled;
        }
    }
    state.push_message(MessageRole::System, format!("theme set to {display_name}"));
    CommandResult::Handled
}

fn cmd_mode(state: &mut AppState, args: &str) -> CommandResult {
    let name = args.trim().to_lowercase();
    match name.as_str() {
        "fullscreen" => state.screen_mode = ScreenMode::Fullscreen,
        "inline" => state.screen_mode = ScreenMode::Inline,
        _ => {
            state.push_message(
                MessageRole::System,
                "usage: /mode <fullscreen|inline>".to_string(),
            );
            return CommandResult::Handled;
        }
    }
    state.push_message(
        MessageRole::System,
        format!("viewport mode set to {name}; switch takes effect next redraw"),
    );
    CommandResult::Handled
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_empty_query_returns_builtins_with_help_first() {
        let all = suggestions("", &[]);
        // 9 builtins, no skills, but empty query is capped at MAX_SUGGESTIONS (8).
        assert_eq!(all.len(), 8);
        assert_eq!(all[0].name, "help");
    }

    #[test]
    fn suggestions_empty_query_caps_at_max() {
        // With 9 builtins + 5 skills = 14 commands, the empty-query menu
        // must cap at MAX_SUGGESTIONS (8), builtins first.
        let skills = vec![
            test_skill("alpha", "a"),
            test_skill("beta", "b"),
            test_skill("gamma", "g"),
            test_skill("delta", "d"),
            test_skill("epsilon", "e"),
        ];
        let all = suggestions("", &skills);
        assert_eq!(all.len(), 8);
        // First 8 are builtins, so no skill appears (mode is the 9th builtin).
        assert!(all.iter().all(|c| !c.name.starts_with("skills:")));
    }

    #[test]
    fn suggestions_skills_prefix_matches_all_skills() {
        let skills = vec![test_skill("clarify", "clarify requirements")];
        // "skills:" prefix matches the skill command (prefix score 80).
        // The `skills` builtin also matches because its description contains
        // "skills:" — substring score 20 — so it appears lower in the list.
        let got = suggestions("skills:", &skills);
        assert!(!got.is_empty());
        assert_eq!(got[0].name, "skills:clarify");
    }

    #[test]
    fn suggestions_skill_name_search() {
        let skills = vec![
            test_skill("clarify", "clarify requirements"),
            test_skill("deploy", "deploy to staging"),
        ];
        // Typing "skills:cla" should narrow to just clarify.
        let got = suggestions("skills:cla", &skills);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "skills:clarify");
    }

    #[test]
    fn dispatch_builtin_returns_handled() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/help");
        assert!(matches!(result, CommandResult::Handled));
        // Echo + system message.
        assert_eq!(state.messages().len(), 2);
    }

    #[test]
    fn dispatch_theme_sets_default() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/theme default");
        assert!(matches!(result, CommandResult::Handled));
        // Echo + confirmation.
        assert_eq!(state.messages().len(), 2);
        assert!(
            state
                .messages()
                .last()
                .unwrap()
                .content
                .contains("theme set")
        );
        assert_eq!(state.theme, Theme::default());
    }

    #[test]
    fn dispatch_theme_unknown_shows_hint() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/theme neon");
        assert!(matches!(result, CommandResult::Handled));
        assert!(
            state
                .messages()
                .last()
                .unwrap()
                .content
                .contains("unknown theme")
        );
    }

    #[test]
    fn dispatch_skill_returns_send_to_agent() {
        let mut state = AppState {
            loaded_skills: vec![test_skill("clarify", "clarify requirements")],
            ..Default::default()
        };
        let result = dispatch(&mut state, "/skills:clarify help me with X");
        match result {
            CommandResult::SendToAgent { message } => {
                assert_eq!(message, "help me with X");
            }
            other => panic!("expected SendToAgent, got {other:?}"),
        }
        // Echo + compact skill indicator; full body is no longer shown.
        assert_eq!(state.messages().len(), 2);
        assert_eq!(state.messages()[0].role, MessageRole::User);
        assert_eq!(state.messages()[1].role, MessageRole::System);
        assert_eq!(state.messages()[1].content, "[skill: skills:clarify]");
    }

    #[test]
    fn dispatch_skill_no_args_sends_placeholder() {
        let mut state = AppState {
            loaded_skills: vec![test_skill("clarify", "clarify requirements")],
            ..Default::default()
        };
        let result = dispatch(&mut state, "/skills:clarify");
        match result {
            CommandResult::SendToAgent { message } => {
                assert_eq!(message, "follow the skill instructions above");
            }
            other => panic!("expected SendToAgent, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_path_like_falls_through() {
        let mut state = AppState::default();
        assert!(matches!(
            dispatch(&mut state, "/tmp/foo"),
            CommandResult::NotACommand
        ));
        assert!(state.messages().is_empty());
    }

    #[test]
    fn dispatch_unknown_shows_help_hint() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/nope");
        assert!(matches!(result, CommandResult::Handled));
        assert!(
            state.messages()[0]
                .content
                .contains("unknown command '/nope'")
        );
    }

    #[test]
    fn dispatch_skills_path_not_treated_as_filesystem() {
        // "skills:clarify" contains ':' but not '/', so looks_like_path
        // must not swallow it.  It also must not be treated as a path
        // because of the colon.
        let mut state = AppState {
            loaded_skills: vec![test_skill("clarify", "clarify")],
            ..Default::default()
        };
        let result = dispatch(&mut state, "/skills:clarify");
        assert!(matches!(result, CommandResult::SendToAgent { .. }));
    }

    #[test]
    fn cmd_skills_lists_loaded_skills() {
        let mut state = AppState {
            loaded_skills: vec![
                test_skill("clarify", "clarify requirements"),
                test_skill("deploy", "deploy to staging"),
            ],
            ..Default::default()
        };
        cmd_skills(&mut state, "");
        let msg = state.messages().last().unwrap();
        assert!(msg.content.contains("/skills:clarify"));
        assert!(msg.content.contains("/skills:deploy"));
    }

    #[test]
    fn cmd_help_includes_skill_section() {
        let mut state = AppState {
            loaded_skills: vec![test_skill("clarify", "clarify requirements")],
            ..Default::default()
        };
        cmd_help(&mut state, "");
        let msg = state.messages().last().unwrap();
        assert!(msg.content.contains("/skills:clarify"));
    }

    #[tokio::test]
    async fn dispatch_goal_start_creates_goal() {
        let mut state = AppState {
            session_key: "agent:main:dm:cli:default:direct:peer-1".to_string(),
            ..Default::default()
        };
        let result = dispatch(&mut state, "/goal get CI green");
        assert!(matches!(result, CommandResult::Handled));
        assert!(state.goal.is_some());
        assert_eq!(state.goal.as_ref().unwrap().objective, "get CI green");
        let msg = state.messages().last().unwrap();
        assert!(msg.content.contains("Goal set"));
    }

    #[tokio::test]
    async fn dispatch_goal_show_shows_summary() {
        let mut state = AppState {
            session_key: "agent:main:dm:cli:default:direct:peer-1".to_string(),
            goal: Some(crate::goal::Goal::new("fix bug")),
            ..Default::default()
        };
        let result = dispatch(&mut state, "/goal");
        assert!(matches!(result, CommandResult::Handled));
        let msg = state.messages().last().unwrap();
        assert!(msg.content.contains("fix bug"));
        assert!(msg.content.contains("Status: active"));
    }

    #[tokio::test]
    async fn dispatch_goal_complete_clears_and_keeps_terminal() {
        let mut state = AppState {
            session_key: "agent:main:dm:cli:default:direct:peer-1".to_string(),
            goal: Some(crate::goal::Goal::new("fix bug")),
            ..Default::default()
        };
        let result = dispatch(&mut state, "/goal complete");
        assert!(matches!(result, CommandResult::Handled));
        assert_eq!(
            state.goal.as_ref().unwrap().status,
            crate::goal::GoalStatus::Complete
        );
    }

    #[test]
    fn dispatch_loop_returns_schedule_loop() {
        let mut state = AppState::default();
        let result = dispatch(&mut state, "/loop 5m check the deploy");
        match result {
            CommandResult::ScheduleLoop { interval, prompt } => {
                assert_eq!(interval, "*/5 * * * *");
                assert_eq!(prompt, "check the deploy");
            }
            other => panic!("expected ScheduleLoop, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_loop_unclean_interval_shows_usage() {
        let mut state = AppState::default();
        // 90m does not map cleanly to a cron expression, so interval_to_cron fails.
        let result = dispatch(&mut state, "/loop 90m check");
        assert!(matches!(result, CommandResult::Handled));
        let msg = state.messages().last().unwrap();
        assert!(msg.content.contains("Usage: /loop"));
    }

    fn test_skill(name: &str, desc: &str) -> legion_skills::Skill {
        legion_skills::Skill {
            frontmatter: legion_skills::SkillFrontmatter {
                name: name.into(),
                description: desc.into(),
                when_to_use: None,
                allowed_tools: vec![],
                paths: vec![],
                user_invocable: true,
                model: None,
                effort: None,
            },
            body: "skill body text".into(),
            source: legion_skills::SkillSource::Workspace,
            path: "/tmp/SKILL.md".into(),
        }
    }
}
