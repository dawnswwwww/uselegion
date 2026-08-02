# TUI Interaction & Visual Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the five ranked gaps found in the TUI analysis: (1) finish the theme system, (2) add run cancel / message queue / newline insertion / paste gating, (3) add live visual feedback (spinner, scroll & queue indicators, copy notice, tables), (4) persist preferences and align status/docs, (5) fix the OSC 8 wrap, code-block background, paste-placeholder, and inline-mode bugs.

**Architecture:** All work is inside `crates/legion-cli` (plus one small config addition in `crates/legion-core`). The existing `Theme` struct is extended and made name-addressable; a new `tui` config section persists `/theme` and `/mode`. Interaction features hook the existing `OutboundControl` channel and the modal key-dispatch chain in `events.rs`. Bug fixes are localized to `render.rs` wrapping, `markdown.rs` code blocks, `input.rs` paste store, and `inline.rs` scrollback rendering.

**Tech Stack:** Rust (MSRV 1.86, edition 2024), ratatui 0.29, crossterm 0.28, tui-textarea 0.7, pulldown-cmark, syntect.

## Global Constraints

- No new dependencies. Everything below uses crates already in `crates/legion-cli/Cargo.toml`.
- Do not touch `claude-code-analysis/`.
- Serde config fields are `camelCase` (`#[serde(rename_all = "camelCase")]`).
- Avoid `unwrap`/`expect` in production code (test code may use them).
- Unit tests live in the same file under `#[cfg(test)]`, matching existing style.
- Fast iteration gate per task: `cargo test -p legion-cli` (+ `cargo check -p legion-core` when that crate is touched). Final gate after each phase: `cargo build --workspace --all-targets && cargo clippy --workspace --all-targets && cargo test --workspace --all-targets && cargo fmt -- --check`.
- **Git commits require the user's explicit confirmation each time.** Commit steps below are written out, but the executing agent MUST skip them unless the user has confirmed commits for this work. Never commit unconfirmed.

---

## Phase 1 — Theme system completion

### Task 1.1: Extend `Theme` (new fields, real light theme, name registry)

**Files:**
- Modify: `crates/legion-cli/src/tui/theme.rs` (full rewrite, currently 56 lines)

**Interfaces:**
- Produces: `Theme::NAMES: &'static [&'static str]`, `Theme::by_name(name: &str) -> Option<Theme>`, `Theme::default_dark()`, `Theme::default_light()`, `Theme::heading_color(&self, level: u8) -> Color`. New fields consumed by Task 1.2: `system_bg`, `question_bg`, `selected_bg`, `code_fg`, `code_gutter_fg`, `inline_code_fg`, `headings: [Color; 6]`, `syntect_theme: &'static str`. `input_border` keeps its name but gains its real meaning (input border color) in Task 3.3.

- [ ] **Step 1: Write the failing tests** (append to `theme.rs`, replacing the file in Step 3)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_name_resolves_known_themes() {
        assert!(Theme::by_name("default").is_some());
        assert!(Theme::by_name("dark").is_some());
        assert!(Theme::by_name("light").is_some());
        assert!(Theme::by_name("solarized").is_none());
    }

    #[test]
    fn light_theme_differs_from_dark() {
        assert_ne!(Theme::default_light(), Theme::default_dark());
    }

    #[test]
    fn names_lists_all_resolvable_themes() {
        for name in Theme::NAMES {
            assert!(Theme::by_name(name).is_some(), "{name} must resolve");
        }
    }

    #[test]
    fn heading_color_is_clamped_for_all_levels() {
        let theme = Theme::default();
        // Levels 1..=6 map to the array; out-of-range levels clamp, not panic.
        let _ = theme.heading_color(1);
        let _ = theme.heading_color(6);
        let _ = theme.heading_color(0);
        let _ = theme.heading_color(255);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli tui::theme`
Expected: FAIL — `by_name`, `NAMES`, `heading_color` do not exist.

- [ ] **Step 3: Rewrite `theme.rs`**

```rust
//! TUI color theme.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub user_bar: Color,
    pub user_bg: Color,
    pub assistant_bar: Color,
    pub assistant_bg: Color,
    pub system_bar: Color,
    pub system_bg: Color,
    pub tool_bar: Color,
    pub question_bar: Color,
    pub question_bg: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub input_border: Color,
    pub selected_fg: Color,
    pub selected_bg: Color,
    pub code_bg: Color,
    pub code_fg: Color,
    pub code_gutter_fg: Color,
    pub code_inline_bg: Color,
    pub inline_code_fg: Color,
    pub link_fg: Color,
    pub error_fg: Color,
    pub spinner_fg: Color,
    /// Foreground colors for heading levels 1-6.
    pub headings: [Color; 6],
    /// syntect theme name used for code-block highlighting. Must exist in
    /// syntect's default `ThemeSet` (`syntax.rs` falls back to the dark
    /// theme when it does not).
    pub syntect_theme: &'static str,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_bar: Color::Cyan,
            user_bg: Color::Rgb(45, 45, 55),
            assistant_bar: Color::Green,
            assistant_bg: Color::Rgb(28, 34, 28),
            system_bar: Color::Yellow,
            system_bg: Color::Rgb(42, 40, 26),
            tool_bar: Color::DarkGray,
            question_bar: Color::Magenta,
            question_bg: Color::Rgb(48, 36, 48),
            status_bg: Color::Rgb(40, 40, 50),
            status_fg: Color::Gray,
            input_border: Color::Blue,
            selected_fg: Color::Black,
            selected_bg: Color::Blue,
            code_bg: Color::Rgb(30, 30, 30),
            code_fg: Color::Rgb(220, 220, 220),
            code_gutter_fg: Color::DarkGray,
            code_inline_bg: Color::Rgb(40, 40, 40),
            inline_code_fg: Color::White,
            link_fg: Color::LightBlue,
            error_fg: Color::Red,
            spinner_fg: Color::Green,
            headings: [
                Color::LightGreen,
                Color::Green,
                Color::Cyan,
                Color::Yellow,
                Color::Magenta,
                Color::DarkGray,
            ],
            syntect_theme: "base16-ocean.dark",
        }
    }
}

impl Theme {
    /// All theme names accepted by `by_name`, for help text and validation.
    pub const NAMES: &'static [&'static str] = &["default", "dark", "light"];

    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "default" | "dark" => Some(Self::default_dark()),
            "light" => Some(Self::default_light()),
            _ => None,
        }
    }

    pub fn default_dark() -> Self {
        Self::default()
    }

    pub fn default_light() -> Self {
        Self {
            user_bar: Color::Rgb(0, 100, 160),
            user_bg: Color::Rgb(225, 235, 245),
            assistant_bar: Color::Rgb(20, 120, 50),
            assistant_bg: Color::Rgb(232, 242, 232),
            system_bar: Color::Rgb(150, 110, 0),
            system_bg: Color::Rgb(245, 240, 220),
            tool_bar: Color::DarkGray,
            question_bar: Color::Rgb(140, 40, 140),
            question_bg: Color::Rgb(242, 230, 242),
            status_bg: Color::Rgb(220, 220, 228),
            status_fg: Color::Rgb(80, 80, 90),
            input_border: Color::Rgb(0, 100, 160),
            selected_fg: Color::White,
            selected_bg: Color::Rgb(0, 100, 160),
            code_bg: Color::Rgb(240, 240, 240),
            code_fg: Color::Rgb(40, 40, 40),
            code_gutter_fg: Color::Rgb(150, 150, 150),
            code_inline_bg: Color::Rgb(230, 230, 230),
            inline_code_fg: Color::Rgb(30, 30, 30),
            link_fg: Color::Rgb(0, 90, 180),
            error_fg: Color::Rgb(180, 30, 30),
            spinner_fg: Color::Rgb(20, 120, 50),
            headings: [
                Color::Rgb(20, 120, 50),
                Color::Rgb(20, 120, 50),
                Color::Rgb(0, 100, 140),
                Color::Rgb(150, 110, 0),
                Color::Rgb(140, 40, 140),
                Color::DarkGray,
            ],
            syntect_theme: "InspiredGitHub",
        }
    }

    /// Foreground color for a markdown heading of `level` (1-6); out-of-range
    /// levels clamp to the nearest entry.
    pub fn heading_color(&self, level: u8) -> Color {
        let idx = (level as usize).saturating_sub(1).min(self.headings.len() - 1);
        self.headings[idx]
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli tui::theme`
Expected: 4 passed. (Compilation of the rest of the crate will fail until Task 1.2 wires the new fields — that is expected; verify with `cargo test -p legion-cli tui::theme 2>&1 | head` that failures are only the known call sites listed in Task 1.2. If you prefer, land Steps 3-4 together with Task 1.2 and run the gate once.)

- [ ] **Step 5: Commit (only if the user has confirmed commits)**

```bash
git add crates/legion-cli/src/tui/theme.rs
git commit -m "feat(tui): extend Theme with light palette and name registry"
```

### Task 1.2: Route all hardcoded colors through `Theme`

**Files:**
- Modify: `crates/legion-cli/src/tui/widgets.rs:26-37`
- Modify: `crates/legion-cli/src/tui/markdown.rs:208-218, 279-288, 318-324, 353-413`
- Modify: `crates/legion-cli/src/tui/syntax.rs:33-41`
- Modify: `crates/legion-cli/src/tui/render.rs:353-358, 390-395`

**Interfaces:**
- Consumes: everything from Task 1.1.
- Produces: `markdown::effective_style(style: Style, in_heading: Option<u8>, theme: &Theme) -> Style` (new third parameter; all call sites in `markdown.rs` updated). The free function `markdown::heading_color` is deleted.

- [ ] **Step 1: Write the failing tests** (append to `crates/legion-cli/src/tui.rs` test module, next to the existing markdown tests around line 740)

```rust
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
    let lines = markdown::markdown_lines("```rust\nlet x = 1;\n```", &light, highlighter(), 80);
    assert!(lines.len() >= 3);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli heading_uses_theme_color role_backgrounds_come_from_theme`
Expected: FAIL — `role_background` still returns hardcoded values; `heading_color` free function still in use.

- [ ] **Step 3: Implement**

1. `widgets.rs` `role_background` (lines 26-37) — delete the stale comment and use theme fields:

```rust
/// Background tint applied to each line of a message to visually group it.
pub(crate) fn role_background(role: MessageRole, theme: &Theme) -> Color {
    match role {
        MessageRole::User => theme.user_bg,
        MessageRole::Assistant => theme.assistant_bg,
        MessageRole::System => theme.system_bg,
        MessageRole::Tool => Color::Reset,
        MessageRole::Question => theme.question_bg,
    }
}
```

2. `markdown.rs`:
   - Delete the free function `heading_color` (lines 279-288).
   - Change `effective_style` to take the theme:

```rust
pub(crate) fn effective_style(style: Style, in_heading: Option<u8>, theme: &Theme) -> Style {
    if let Some(level) = in_heading {
        style.fg(theme.heading_color(level)).add_modifier(Modifier::BOLD)
    } else {
        style
    }
}
```

   - Update every `effective_style(style, in_heading)` call in `markdown_lines` to `effective_style(style, in_heading, theme)` (5 call sites: lines 113, 184, 190, 213, and inside `flush_pending` — for `flush_pending`, add a `theme: &Theme` parameter and pass it through from its ~12 call sites).
   - Inline code span (line 214-217): replace `Color::White` with `theme.inline_code_fg`:

```rust
current_spans.push(Span::styled(
    content.to_string(),
    Style::default().fg(theme.inline_code_fg).bg(theme.code_inline_bg),
));
```

   - `emit_code_block` (lines 363-366): replace the two hardcoded colors:

```rust
let code_style = Style::default().bg(theme.code_bg).fg(theme.code_fg);
let gutter_style = Style::default().bg(theme.code_bg).fg(theme.code_gutter_fg);
```

3. `syntax.rs` `highlight_lines` (lines 33-41): use the theme's syntect theme with a safe fallback, renaming `_theme` to `theme`:

```rust
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
    // ... rest unchanged ...
```

4. `render.rs`: in both the slash-completion menu (lines 353-358) and the history-search popup (lines 390-395), replace `.bg(theme.input_border)` with `.bg(theme.selected_bg)`.

- [ ] **Step 4: Run tests and clippy**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/tui/{widgets.rs,markdown.rs,syntax.rs,render.rs} crates/legion-cli/src/tui.rs
git commit -m "refactor(tui): route remaining hardcoded colors through Theme"
```

### Task 1.3: `tui` config section + persist `/theme` and `/mode`

**Files:**
- Modify: `crates/legion-core/src/config.rs:75-80` (add field) and `:893` area (add struct)
- Modify: `crates/legion-cli/src/tui/state.rs:44-51, 94-189` (ScreenMode helpers + two AppState fields)
- Modify: `crates/legion-cli/src/tui.rs:92-98` (apply config at startup)
- Modify: `crates/legion-cli/src/slash_commands.rs:506-543` (`cmd_theme`, `cmd_mode`)

**Interfaces:**
- Produces: `legion_core::config::TuiConfig { theme: String, screen_mode: String }` on `Config.tui` (JSON keys `tui.theme`, `tui.screenMode`). `ScreenMode::from_name(&str) -> Option<ScreenMode>`, `ScreenMode::name(&self) -> &'static str`. `AppState.theme_name: String`, `AppState.config_path: Option<std::path::PathBuf>`. Slash-command helper `persist_tui_config(state: &AppState, key: &str, value: &str) -> bool` (returns true when the write happened; false when `config_path` is `None` — which is always the case in tests — or the write failed).

- [ ] **Step 1: Write the failing tests**

`crates/legion-core/src/config.rs` test module (append):

```rust
#[test]
fn tui_config_defaults_when_absent() {
    let config = Config::from_json(r#"{"gateway": {"auth": {"mode": "token", "token": "x"}}}"#)
        .expect("config must parse");
    assert_eq!(config.tui.theme, "default");
    assert_eq!(config.tui.screen_mode, "fullscreen");
}

#[test]
fn tui_config_parses_camel_case() {
    let config = Config::from_json(
        r#"{"gateway": {"auth": {"mode": "token", "token": "x"}}, "tui": {"theme": "light", "screenMode": "inline"}}"#,
    )
    .expect("config must parse");
    assert_eq!(config.tui.theme, "light");
    assert_eq!(config.tui.screen_mode, "inline");
}
```

(If the minimal gateway JSON above does not parse, copy the minimal valid config shape from an existing `config.rs` test.)

`crates/legion-cli/src/tui.rs` test module (append):

```rust
#[test]
fn screen_mode_name_roundtrip() {
    assert_eq!(ScreenMode::from_name("fullscreen"), Some(ScreenMode::Fullscreen));
    assert_eq!(ScreenMode::from_name("inline"), Some(ScreenMode::Inline));
    assert_eq!(ScreenMode::from_name("bogus"), None);
    assert_eq!(ScreenMode::Fullscreen.name(), "fullscreen");
    assert_eq!(ScreenMode::Inline.name(), "inline");
}

#[test]
fn theme_command_does_not_persist_without_config_path() {
    let mut state = AppState::default(); // config_path is None in tests
    let result = crate::slash_commands::dispatch(&mut state, "/theme light");
    assert!(matches!(result, crate::slash_commands::CommandResult::Handled));
    assert_eq!(state.theme, crate::tui::theme::Theme::default_light());
    assert_eq!(state.theme_name, "light");
    let last = state.messages().last().expect("feedback message");
    assert!(last.content.contains("theme set to light"));
    assert!(!last.content.contains("saved"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-core tui_config && cargo test -p legion-cli screen_mode_name_roundtrip theme_command`
Expected: FAIL — `config.tui`, `ScreenMode::from_name`, `AppState.theme_name` do not exist.

- [ ] **Step 3: Implement**

1. `crates/legion-core/src/config.rs` — add to `Config` after the `todos` field (line 77):

```rust
    /// TUI display preferences (theme, viewport mode), persisted by the
    /// `/theme` and `/mode` slash commands.
    #[serde(default)]
    pub tui: TuiConfig,
```

and add the struct next to `TodosConfig` (after line 893):

```rust
/// TUI display preferences.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TuiConfig {
    /// Color theme name; see `Theme::by_name` in legion-cli ("default", "dark", "light").
    #[serde(default = "default_tui_theme")]
    pub theme: String,
    /// Viewport mode: "fullscreen" or "inline".
    #[serde(default = "default_tui_screen_mode")]
    pub screen_mode: String,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: default_tui_theme(),
            screen_mode: default_tui_screen_mode(),
        }
    }
}

fn default_tui_theme() -> String {
    "default".to_string()
}

fn default_tui_screen_mode() -> String {
    "fullscreen".to_string()
}
```

2. `crates/legion-cli/src/tui/state.rs` — add `ScreenMode` helpers after the enum (line 51):

```rust
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
```

Add two `AppState` fields (near `screen_mode`, line 184):

```rust
    /// Name of the active theme (for `/status` and persistence).
    pub(crate) theme_name: String,
    /// Config file path used to persist `/theme` and `/mode`. `None` in
    /// tests, where persistence must not touch the real config file.
    pub(crate) config_path: Option<std::path::PathBuf>,
```

3. `crates/legion-cli/src/tui.rs` — in `run_tui`, after the `state_inner` literal (line 98):

```rust
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
```

4. `crates/legion-cli/src/slash_commands.rs` — add the persistence helper above `cmd_theme`:

```rust
/// Persist a `tui.*` config value. Returns false when there is no config
/// path (tests) or the write fails; failures are logged, not fatal.
fn persist_tui_config(state: &AppState, key: &str, value: &str) -> bool {
    let Some(path) = state.config_path.clone() else {
        return false;
    };
    let key = key.to_string();
    let raw = format!("\"{value}\"");
    match crate::config_set(&path, &key, &raw) {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(error = %err, key = %key, "failed to persist TUI config");
            false
        }
    }
}
```

Rewrite `cmd_theme`:

```rust
fn cmd_theme(state: &mut AppState, args: &str) -> CommandResult {
    let name = {
        let trimmed = args.trim().to_lowercase();
        if trimmed.is_empty() { "default".to_string() } else { trimmed }
    };
    match Theme::by_name(&name) {
        Some(theme) => {
            state.theme = theme;
            state.theme_name = name.clone();
            let saved = persist_tui_config(state, "tui.theme", &name);
            let suffix = if saved { " · saved" } else { "" };
            state.push_message(MessageRole::System, format!("theme set to {name}{suffix}"));
        }
        None => {
            state.push_message(
                MessageRole::System,
                format!(
                    "unknown theme '/theme {name}' — try one of: {}",
                    Theme::NAMES.join(", ")
                ),
            );
        }
    }
    CommandResult::Handled
}
```

Rewrite `cmd_mode`:

```rust
fn cmd_mode(state: &mut AppState, args: &str) -> CommandResult {
    let name = args.trim().to_lowercase();
    match ScreenMode::from_name(&name) {
        Some(mode) => {
            state.screen_mode = mode;
            let saved = persist_tui_config(state, "tui.screenMode", mode.name());
            let suffix = if saved { " · saved" } else { "" };
            state.push_message(
                MessageRole::System,
                format!("viewport mode set to {}; takes effect next redraw{suffix}", mode.name()),
            );
        }
        None => {
            state.push_message(
                MessageRole::System,
                "usage: /mode <fullscreen|inline>".to_string(),
            );
        }
    }
    CommandResult::Handled
}
```

Check `slash_commands.rs` imports: `Theme` and `ScreenMode` are already imported (current `cmd_theme`/`cmd_mode` use them). Update any existing tests in `slash_commands.rs` that assert the old "try dark, light, or default" message (grep for it; the new wording is "try one of: default, dark, light").

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-core -p legion-cli && cargo clippy -p legion-core -p legion-cli --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 5: Phase 1 final gate + commit (commit only if confirmed)**

```bash
cargo build --workspace --all-targets && cargo clippy --workspace --all-targets && cargo test --workspace --all-targets && cargo fmt -- --check
git add crates/legion-core/src/config.rs crates/legion-cli/src/
git commit -m "feat(tui): persist theme and viewport mode in tui config section"
```

---

## Phase 2 — Interaction essentials (cancel / queue / newline / paste gating)

### Task 2.1: Esc cancels the in-flight run (local mode)

**Files:**
- Modify: `crates/legion-cli/src/tui/state.rs:79-92` (`OutboundControl`)
- Modify: `crates/legion-cli/src/driver.rs:84-110` (trait), `:230-249` area (WsDriver impl), `:297-352` (LocalDriver struct + `new`), `:354-474` (LocalDriver impl)
- Modify: `crates/legion-cli/src/tui.rs:323-357` (sender task)
- Modify: `crates/legion-cli/src/tui/events.rs:236-239` (Esc arm)

**Interfaces:**
- Produces: `OutboundControl::Cancel`; `TurnDriver::cancel(&self) -> Result<(), CliError>` (async). LocalDriver gains a private field `current_run: Mutex<Option<tokio::task::JoinHandle<()>>>`.

- [ ] **Step 1: Write the failing tests** (append to `crates/legion-cli/src/tui.rs` test module, following the existing `handle_key_event` test pattern)

```rust
#[test]
fn esc_during_active_run_sends_cancel() {
    let mut state = AppState::default();
    state.pending_request = true; // run in flight, no first token yet
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
```

Note: `OutboundControl` already derives `PartialEq` (`state.rs:78`), so `assert_eq!` works.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli esc_during_active_run esc_when_idle`
Expected: FAIL — `OutboundControl::Cancel` does not exist.

- [ ] **Step 3: Implement**

1. `state.rs` — add the variant to `OutboundControl` (after `Message(String)`, line 80):

```rust
    /// Cancel the in-flight run (Esc while the agent is working).
    Cancel,
```

2. `driver.rs` — add to the `TurnDriver` trait (after `run_turn`, line 88):

```rust
    /// Cancel the in-flight run, if any. Embedded mode aborts the drive
    /// task and emits a synthetic lifecycle error frame so the TUI resets.
    /// The gateway has no cancel RPC yet, so the WS driver returns an error.
    async fn cancel(&self) -> Result<(), CliError>;
```

`WsDriver` impl (after its `run_turn`, line 162):

```rust
    async fn cancel(&self) -> Result<(), CliError> {
        Err(CliError::Other(
            "cancel is not supported in gateway mode; the run continues on the gateway".to_string(),
        ))
    }
```

`LocalDriver`: add the field to the struct (after `current_question_gate`, line 314):

```rust
    /// Drive task of the in-flight turn, aborted by `cancel`.
    current_run: Mutex<Option<tokio::task::JoinHandle<()>>>,
```

Initialize it in `LocalDriver::new`'s struct literal (line 340 area): `current_run: Mutex::new(None),`.

In `LocalDriver::run_turn`, capture the handle (change line 393-410):

```rust
        let event_tx = self.event_tx.clone();
        // Drive the run in the background so the TUI stays responsive;
        // events arrive on the same channel the WS reader would use. The
        // handle is stored so `cancel` can abort the turn.
        let handle = tokio::spawn(async move {
            if let Err(err) = legion_host::drive_run_stream(
                stream,
                session_store,
                session_key,
                text,
                run_id,
                move |frame| {
                    if let Ok(value) = serde_json::to_value(&frame) {
                        let _ = event_tx.send(value);
                    }
                },
            )
            .await
            {
                tracing::error!(error = %err, "failed to persist session transcript");
            }
        });
        *lock_recover(&self.current_run) = Some(handle);
        Ok(())
```

`LocalDriver::cancel` impl (after its `resolve_question`):

```rust
    async fn cancel(&self) -> Result<(), CliError> {
        let handle = lock_recover(&self.current_run).take();
        // Drop the gates so a late resolve cannot target a cancelled turn.
        *lock_recover(&self.current_gate) = None;
        *lock_recover(&self.current_question_gate) = None;
        if let Some(handle) = handle {
            handle.abort();
            // Synthetic lifecycle frame: the TUI's existing error handler
            // resets pending_request and marks the turn as failed.
            let _ = self.event_tx.send(json!({
                "type": "event",
                "event": "agent",
                "payload": {
                    "stream": "lifecycle",
                    "phase": "error",
                    "error": "cancelled by user"
                }
            }));
        }
        Ok(())
    }
```

3. `tui.rs` sender task — add the match arm (after `ResolveQuestion`, line 354):

```rust
                state::OutboundControl::Cancel => {
                    if let Err(err) = sender_driver.cancel().await {
                        let mut s = lock_recover(&sender_state);
                        s.messages.push(state::ChatMessage::new(
                            state::MessageRole::System,
                            format!("{err}"),
                        ));
                        drop(s);
                        let _ = wake_tx.send(json!({ "type": "internal", "event": "cancel-failed" }));
                    }
                }
```

4. `events.rs` — add the Esc arm to the normal-mode `match key.code` (before the catch-all `_` arm at line 237):

```rust
        // Esc cancels the in-flight run. When idle it does nothing, matching
        // its previous behavior (tui-textarea ignores Esc).
        KeyCode::Esc => {
            if state.is_active() {
                let _ = send_tx.send(OutboundControl::Cancel);
                state.messages.push(ChatMessage::new(
                    MessageRole::System,
                    "cancelling run…".to_string(),
                ));
            }
        }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS. (Also confirm `cargo check -p legion-cli` compiles the driver changes — any *other* `TurnDriver` impls in the crate must gain `cancel`; grep `impl TurnDriver` to be sure. Tests using a stub driver will need the method too.)

- [ ] **Step 5: Manual verification note** (no code)

LocalDriver cancel is verified end-to-end manually: run `cargo run -p legion-cli -- tui`, send a long prompt, press Esc, expect "cancelling run…" then "run failed: cancelled by user" and the spinner to stop. Record the outcome in the task result.

- [ ] **Step 6: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "feat(tui): Esc cancels the in-flight run in local mode"
```

### Task 2.2: Queue messages while a run is active

**Files:**
- Modify: `crates/legion-cli/src/tui/state.rs:94-189` (AppState field)
- Modify: `crates/legion-cli/src/tui/events.rs:86-181` (Enter paths), `:513-730` (`handle_ws_event` signature + lifecycle arms)
- Modify: `crates/legion-cli/src/tui.rs:491` and the 13 `handle_ws_event` test call sites

**Interfaces:**
- Produces: `AppState.queued_messages: std::collections::VecDeque<(String, bool)>` — `(text, show_in_chat)`. New signature: `events::handle_ws_event(state: &mut AppState, msg: serde_json::Value, send_tx: &mpsc::UnboundedSender<OutboundControl>)`.

- [ ] **Step 1: Write the failing tests** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
#[test]
fn enter_during_active_run_queues_message() {
    let mut state = AppState::default();
    state.pending_request = true; // run in flight
    state.composer.set_text("follow-up question");
    let (tx, mut rx) = mpsc::unbounded_channel::<state::OutboundControl>();
    events::handle_key_event(
        &mut state,
        event::KeyEvent::new(event::KeyCode::Enter, event::KeyModifiers::NONE),
        &tx,
    );
    assert!(rx.try_recv().is_err(), "queued message must not be sent yet");
    assert_eq!(state.queued_messages.len(), 1);
    assert!(
        !state.messages().iter().any(|m| m.content.contains("follow-up")),
        "queued message must not appear in chat before it is sent"
    );
    assert_eq!(state.composer.join(), "");
}

#[test]
fn lifecycle_end_drains_queue() {
    let mut state = AppState::default();
    state.pending_request = true;
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
    assert!(state.pending_request, "the drained message starts a new run");
    let last = state.messages().last().expect("user message in chat");
    assert_eq!(last.content, "next question");
    assert_eq!(last.role, MessageRole::User);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli enter_during_active lifecycle_end_drains`
Expected: FAIL — `queued_messages` and the new `handle_ws_event` signature do not exist.

- [ ] **Step 3: Implement**

1. `state.rs` — add the field (near `pending_request`, line 135):

```rust
    /// User messages typed while a run is active, sent (in order) when the
    /// run finishes. The bool marks whether the text should appear in the
    /// chat as a user message when it is finally sent (false for
    /// agent-directed slash-command payloads).
    pub(crate) queued_messages: std::collections::VecDeque<(String, bool)>,
```

2. `events.rs` — add two helpers above `handle_key_event`:

```rust
/// Send a user-typed message now, or queue it behind the in-flight run.
/// Queued messages do not appear in the chat until they are actually sent,
/// so streaming deltas always append to the last assistant message.
fn send_user_message(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
    text: String,
) {
    if state.is_active() {
        state.queued_messages.push_back((text, true));
    } else {
        state
            .messages
            .push(ChatMessage::new(MessageRole::User, text.clone()));
        state.pending_request = true;
        let _ = send_tx.send(OutboundControl::Message(text));
    }
}

/// Send an agent-directed payload (slash-command/skill bodies): never
/// rendered in the chat, but still serialized behind an in-flight run.
fn send_agent_message(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
    message: String,
) {
    if state.is_active() {
        state.queued_messages.push_back((message, false));
    } else {
        state.pending_request = true;
        let _ = send_tx.send(OutboundControl::Message(message));
    }
}

/// Pop the oldest queued message into the chat and send it. Called when a
/// run lifecycle ends (normally, with an error, or cancelled).
fn drain_queued_message(
    state: &mut AppState,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
    if let Some((text, show_in_chat)) = state.queued_messages.pop_front() {
        if show_in_chat {
            state
                .messages
                .push(ChatMessage::new(MessageRole::User, text.clone()));
        }
        state.pending_request = true;
        let _ = send_tx.send(OutboundControl::Message(text));
    }
}
```

3. In `handle_key_event`'s Enter arm, replace the send sites:
   - `CommandResult::SendToAgent { message }` (two places, lines 95-98 and 142-146): replace `state.pending_request = true; let _ = send_tx.send(OutboundControl::Message(message));` with `send_agent_message(state, send_tx, message);`
   - The `NotACommand` fall-through (lines 155-163): keep the `crate::tui::input::commit_and_clear_input(state, &text);` call, and replace the push-user-message + `pending_request = true` + `send` lines with `send_user_message(state, send_tx, text);`
   - The plain-message branch (lines 165-178): replace the push + comment + send block with:

```rust
                    } else {
                        // Queued behind an in-flight run when necessary. No
                        // empty assistant placeholder is added here either
                        // way; the assistant row is created lazily by
                        // handle_ws_event when the first delta arrives.
                        crate::tui::input::commit_and_clear_input(state, &text);
                        send_user_message(state, send_tx, text);
                    }
```

   Keep the existing comment about not adding an empty assistant placeholder (move it above `send_user_message`).

4. `handle_ws_event`: change the signature to

```rust
pub(crate) fn handle_ws_event(
    state: &mut AppState,
    msg: serde_json::Value,
    send_tx: &mpsc::UnboundedSender<OutboundControl>,
) {
```

and call `drain_queued_message(state, send_tx);` at the end of both the `Some("end")` arm (after line 594's block) and the `Some("error")` arm (after line 615's block).

5. `tui.rs`: update the real call site (line 491) to pass `&send_tx` (`send_tx` is in scope in `tui_loop`). Update the 13 test call sites: in each test add `let (tx, _rx) = mpsc::unbounded_channel::<state::OutboundControl>();` and pass `&tx`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "feat(tui): queue messages typed while a run is active"
```

### Task 2.3: Alt+Enter inserts a newline

**Files:**
- Modify: `crates/legion-cli/src/tui/composer.rs:95-99` area
- Modify: `crates/legion-cli/src/tui/events.rs:86` (Enter arm)
- Modify: `crates/legion-cli/src/tui/widgets.rs:450-467` (shortcuts line)

**Interfaces:**
- Produces: `Composer::insert_newline(&mut self)`.

- [ ] **Step 1: Write the failing test** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli alt_enter plain_enter_still`
Expected: FAIL — Alt+Enter currently sends.

- [ ] **Step 3: Implement**

1. `composer.rs` (after `insert_str`):

```rust
    /// Insert a newline at the cursor (Alt+Enter).
    pub fn insert_newline(&mut self) {
        self.textarea.insert_newline();
    }
```

2. `events.rs` — add a guarded arm *before* the existing `KeyCode::Enter` arm (line 86; the plain arm matches every modifier, so order matters):

```rust
        // Alt+Enter inserts a newline; plain Enter sends. (Shift+Enter is
        // indistinguishable from Enter without the kitty keyboard protocol,
        // which we do not enable.)
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            state.composer.insert_newline();
        }
```

3. `widgets.rs` shortcuts line — replace the `^Enter`/`send` spans and add the new hints:

```rust
    let shortcuts_line = Line::from(vec![
        Span::styled("^Q ", Style::default().fg(theme.tool_bar)),
        Span::raw("quit "),
        Span::styled("Enter ", Style::default().fg(theme.tool_bar)),
        Span::raw("send "),
        Span::styled("Alt+Enter ", Style::default().fg(theme.tool_bar)),
        Span::raw("newline "),
        Span::styled("Esc ", Style::default().fg(theme.tool_bar)),
        Span::raw("cancel "),
        Span::styled("↑/↓ ", Style::default().fg(theme.tool_bar)),
        Span::raw("history "),
        Span::styled("PgUp/PgDn ", Style::default().fg(theme.tool_bar)),
        Span::raw("scroll "),
        Span::styled("/ ", Style::default().fg(theme.tool_bar)),
        Span::raw("commands "),
        Span::styled("Tab ", Style::default().fg(theme.tool_bar)),
        Span::raw("complete"),
    ]);
```

(`^T think` and `Shift+↑/↓ cursor` are dropped to keep the line within typical widths; both still work.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "feat(tui): Alt+Enter inserts a newline in the composer"
```

### Task 2.4: Gate paste events by modal state; fix history-search modifier leak

**Files:**
- Modify: `crates/legion-cli/src/tui/events.rs:352-383` (history search keys) + new `route_paste` function
- Modify: `crates/legion-cli/src/tui.rs:527-529` (paste branch)

**Interfaces:**
- Produces: `events::route_paste(state: &mut AppState, text: String)` — the single entry point for `Event::Paste`.

- [ ] **Step 1: Write the failing tests** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
#[test]
fn paste_during_approval_is_dropped() {
    let mut state = AppState::default();
    state.pending_approval = Some(("p1".to_string(), "exec".to_string()));
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
    let mut state = AppState::default();
    state.history_search = Some(crate::tui::history_search::HistorySearch::new());
    events::route_paste(&mut state, "cargo\nbuild".to_string());
    let hs = state.history_search.as_ref().expect("search still open");
    assert_eq!(hs.query, "cargo build");
    assert_eq!(state.composer.join(), "");
}

#[test]
fn ctrl_char_does_not_leak_into_history_search_query() {
    let mut state = AppState::default();
    state.history_search = Some(crate::tui::history_search::HistorySearch::new());
    events::handle_key_event(
        &mut state,
        event::KeyEvent::new(event::KeyCode::Char('w'), event::KeyModifiers::CONTROL),
        &mpsc::unbounded_channel::<state::OutboundControl>().0,
    );
    let hs = state.history_search.as_ref().expect("search still open");
    assert_eq!(hs.query, "");
}
```

(`HistorySearch` fields `query`/`selected` are `pub(crate)` — visible from `tui.rs` tests since both are in the same crate module tree. If the struct or fields are not visible, adjust to construct via `HistorySearch::new()` and assert through `filtered()`.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli paste_during paste_into_history ctrl_char`
Expected: FAIL — `route_paste` does not exist; Ctrl+W currently pollutes the query.

- [ ] **Step 3: Implement**

1. `events.rs` — add `route_paste` (place it next to `handle_paste` usage; it calls `crate::tui::input::handle_paste`):

```rust
/// Route a bracketed-paste event. Pastes are modal-gated like keys: while an
/// approval or question prompt owns the keyboard the paste is dropped (the
/// input box is invisible, so stuffing text into it is never intended), and
/// while the history-search popup is open the paste extends the query.
pub(crate) fn route_paste(state: &mut AppState, text: String) {
    if let Some(ref mut hs) = state.history_search {
        hs.query.push_str(&text.replace(['\n', '\r'], " "));
        hs.selected = 0;
    } else if state.pending_approval.is_none() && state.pending_question.is_none() {
        crate::tui::input::handle_paste(state, text);
    }
}
```

2. `events.rs` history-search `Char` arm (line 377): guard the modifiers:

```rust
        KeyCode::Char(c)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            hs.query.push(c);
            hs.selected = 0;
        }
```

3. `tui.rs` paste branch (lines 527-529) becomes:

```rust
                Event::Paste(text) => {
                    events::route_paste(&mut lock_recover(&state), text);
                }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Phase 2 final gate + commit (commit only if confirmed)**

```bash
cargo build --workspace --all-targets && cargo clippy --workspace --all-targets && cargo test --workspace --all-targets && cargo fmt -- --check
git add crates/legion-cli/src/
git commit -m "fix(tui): gate paste events by modal state"
```

---

## Phase 3 — Live visual feedback

### Task 3.1: Animated spinner in the status bar

**Files:**
- Modify: `crates/legion-cli/src/tui/state.rs:94-189` (field)
- Modify: `crates/legion-cli/src/tui/widgets.rs:375-401` (status text)
- Modify: `crates/legion-cli/src/tui.rs:539-551` (tick branch)

**Interfaces:**
- Produces: `AppState.spinner_frame: usize`; `widgets::SPINNER: &[char]`.

- [ ] **Step 1: Write the failing test** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
#[test]
fn status_bar_shows_spinner_and_cancel_hint_while_active() {
    let theme = theme();
    let mut state = AppState::default();
    state.pending_request = true;
    state.spinner_frame = 0;
    let lines = widgets::status_bar_lines(&state, &theme, 2);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains(widgets::SPINNER[0]));
    assert!(text.contains("esc to cancel"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p legion-cli status_bar_shows_spinner`
Expected: FAIL — `SPINNER` and `spinner_frame` do not exist.

- [ ] **Step 3: Implement**

1. `state.rs` — add the field:

```rust
    /// Frame counter for the status-bar spinner, advanced by the UI tick
    /// while a run is active.
    pub(crate) spinner_frame: usize,
```

2. `widgets.rs` — add near the top:

```rust
/// Braille spinner frames, indexed by `AppState::spinner_frame`.
pub(crate) const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
```

In `status_bar_lines`, replace the `is_active` branch (line 390-391):

```rust
    } else if state.is_active() {
        let frame = SPINNER[state.spinner_frame % SPINNER.len()];
        (format!("{frame} typing... (esc to cancel)"), theme.system_bar)
    }
```

3. `tui.rs` tick branch (lines 539-551) — advance the frame and force a redraw while active:

```rust
        if last_tick.elapsed() >= tick_rate {
            last_tick = tokio::time::Instant::now();
            let mut s = lock_recover(&state);
            // Expire the todo panel hide timer if all items are completed.
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
            drop(s);
        }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "feat(tui): animate the status-bar spinner while a run is active"
```

### Task 3.2: Chat chrome — "↓ more" and "N queued" indicators

**Files:**
- Modify: `crates/legion-cli/src/tui/render.rs:283-285` (chat block)

**Interfaces:**
- Consumes: `state.queued_messages` (Task 2.2), `state.scroll`/`max_scroll`.
- Produces: nothing new; uses ratatui's `ratatui::widgets::block::{Title, Position}` and `ratatui::layout::Alignment`.

- [ ] **Step 1: Write the failing test** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
#[test]
fn chat_block_shows_scroll_and_queue_indicators() {
    let mut state = AppState::default();
    for i in 0..50 {
        state.push_message(MessageRole::Assistant, format!("line {i}"));
    }
    state.scroll = 0; // scrolled up
    state
        .queued_messages
        .push_back(("later".to_string(), true));
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render::draw_ui(f, &mut state))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
    assert!(content.contains("↓ more"), "scroll indicator missing: {content}");
    assert!(content.contains("1 queued"), "queue indicator missing: {content}");
}
```

(If `render` is not reachable from the test module because it is private to `tui.rs`, the tests in `tui.rs` already access sibling modules via `render::` / `widgets::` paths — mirror an existing test's imports. `Cell::symbol` exists on ratatui 0.29 `Buffer::content`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p legion-cli chat_block_shows`
Expected: FAIL — no indicators rendered today.

- [ ] **Step 3: Implement** (`render.rs`, replacing lines 283-285)

```rust
    let mut chat_block = Block::default().title("Legion").borders(Borders::ALL);
    if state.scroll < max_scroll {
        chat_block = chat_block.title(
            ratatui::widgets::block::Title::from(Span::styled(
                " ↓ more ",
                Style::default().fg(theme.system_bar),
            ))
            .alignment(ratatui::layout::Alignment::Right)
            .position(ratatui::widgets::block::Position::Bottom),
        );
    }
    if !state.queued_messages.is_empty() {
        chat_block = chat_block.title(
            ratatui::widgets::block::Title::from(Span::styled(
                format!(" ⏳ {} queued ", state.queued_messages.len()),
                Style::default().fg(theme.user_bar),
            ))
            .alignment(ratatui::layout::Alignment::Right),
        );
    }
    let chat = Paragraph::new(Text::from(visible_lines)).block(chat_block);
    f.render_widget(chat, chat_area);
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "feat(tui): show scroll position and queued-count indicators on the chat frame"
```

### Task 3.3: Copy feedback, input placeholder, themed input border

**Files:**
- Modify: `crates/legion-cli/src/tui/state.rs:94-189` (notice field)
- Modify: `crates/legion-cli/src/tui/events.rs:26-37` (copy branch)
- Modify: `crates/legion-cli/src/tui/widgets.rs:381-401` (status priority chain)
- Modify: `crates/legion-cli/src/tui.rs:92-98` (placeholder at startup), `:539-551` (notice expiry in tick)
- Modify: `crates/legion-cli/src/tui/composer.rs:112-122` (placeholder + `set_chrome`)
- Modify: `crates/legion-cli/src/tui/render.rs:309-318` (input title/border)

**Interfaces:**
- Produces: `AppState.notice: Option<(String, std::time::Instant)>`; `Composer::set_chrome(&mut self, title: &'static str, border: ratatui::style::Color)` (replaces `set_title`); `widgets::NOTICE_TTL: std::time::Duration` (3s).

- [ ] **Step 1: Write the failing tests** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
#[test]
fn copy_sets_notice_shown_in_status_bar() {
    let theme = theme();
    let mut state = AppState::default();
    state.notice = Some(("copied 42 chars".to_string(), std::time::Instant::now()));
    let lines = widgets::status_bar_lines(&state, &theme, 2);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("copied 42 chars"));
}

#[test]
fn expired_notice_falls_back_to_status() {
    let theme = theme();
    let mut state = AppState::default();
    state.status = "local".to_string();
    state.notice = Some((
        "copied 42 chars".to_string(),
        std::time::Instant::now() - std::time::Duration::from_secs(10),
    ));
    let lines = widgets::status_bar_lines(&state, &theme, 2);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(!text.contains("copied"));
    assert!(text.contains("local"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli copy_sets_notice expired_notice`
Expected: FAIL — `notice` does not exist.

- [ ] **Step 3: Implement**

1. `state.rs`:

```rust
    /// Transient status-bar notice (e.g. "copied N chars"), shown for
    /// `widgets::NOTICE_TTL` instead of the connection status.
    pub(crate) notice: Option<(String, std::time::Instant)>,
```

2. `events.rs` copy branch (after `print!("{}", osc52_copy(&text));`, line 31):

```rust
                state.notice = Some((
                    format!("copied {} chars", text.chars().count()),
                    std::time::Instant::now(),
                ));
```

3. `widgets.rs` — add the TTL constant and rework the status priority chain:

```rust
/// How long a transient notice (copy feedback, ...) replaces the status text.
pub(crate) const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
```

```rust
    let fresh_notice = state
        .notice
        .as_ref()
        .filter(|(_, at)| at.elapsed() < NOTICE_TTL)
        .map(|(text, _)| text.clone());
    let (status_text, status_color) = if let Some(pq) = &state.pending_question {
        let hint = question_hint(pq);
        let header = pq
            .current_question()
            .map(|q| q.header.as_str())
            .unwrap_or(SUBMIT_LABEL);
        (format!("{} ({})", header, hint), theme.system_bar)
    } else if let Some((_, tool)) = &state.pending_approval {
        (format!("approve tool '{tool}'? y/n"), theme.system_bar)
    } else if let Some(notice) = fresh_notice {
        (notice, theme.assistant_bar)
    } else if state.is_active() {
        let frame = SPINNER[state.spinner_frame % SPINNER.len()];
        (format!("{frame} typing... (esc to cancel)"), theme.system_bar)
    } else {
        (
            state.status.clone(),
            if state.status == "connected" {
                theme.assistant_bar
            } else {
                theme.system_bar
            },
        )
    };
```

4. `tui.rs` tick branch — expire stale notices (inside the same `if last_tick.elapsed() >= tick_rate` block, after the spinner advance):

```rust
            if s.notice
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= widgets::NOTICE_TTL)
            {
                s.notice = None;
                dirty = true;
            }
```

(`widgets` is reachable inside `tui.rs`; the test module there already uses `widgets::...`.)

5. `tui.rs` startup — set the composer placeholder right after the `state_inner` literal (line 98):

```rust
    state_inner
        .composer
        .placeholder("type a message · / commands · ! shell");
```

6. `composer.rs` — remove `#[allow(dead_code)]` from `placeholder`, and replace `set_title` with:

```rust
    /// Update the border title and color (e.g. "shell mode" when the input
    /// starts with `!`).
    pub fn set_chrome(&mut self, title: &'static str, border: ratatui::style::Color) {
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(ratatui::style::Style::default().fg(border)),
        );
    }
```

7. `render.rs` input box section (lines 309-318):

```rust
    let (input_title, border_color) = if state.composer.join().starts_with('!') {
        ("shell mode", theme.system_bar)
    } else {
        ("Input", theme.input_border)
    };
    state.composer.set_chrome(input_title, border_color);
    state.composer.render(input_area, f.buffer_mut());
```

Grep for other `set_title` callers and update them (expected: only `render.rs`).

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "feat(tui): copy feedback notice, input placeholder, themed input border"
```

### Task 3.4: Markdown table rendering

**Files:**
- Modify: `crates/legion-cli/src/tui/markdown.rs:23-41` (parser options) + table state handling

**Interfaces:**
- Produces: nothing public; `markdown_lines` gains internal table support via `pulldown_cmark::Options::ENABLE_TABLES`.

- [ ] **Step 1: Write the failing test** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
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
    let header_row = rendered.iter().find(|l| l.contains("Name")).expect("header row");
    let body_row = rendered.iter().find(|l| l.contains("Bob")).expect("body row");
    assert_eq!(
        header_row.find("Age"),
        body_row.find("42"),
        "second column must align"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p legion-cli markdown_table`
Expected: FAIL — tables currently render as raw `|` text, and the `markdown_lines` signature does not yet have the width parameter (added in Step 3 below).

- [ ] **Step 3: Implement** (`markdown.rs`)

0. Add the `viewport_width` parameter now (Task 5.2 will use it; here it is unused, so name it `_viewport_width` to avoid the unused-parameter warning):
   - `markdown_lines(text: &str, theme: &Theme, highlighter: &Highlighter, _viewport_width: u16)`
   - `widgets.rs` `message_lines`: pass its own `_viewport_width` argument through to `markdown_lines` (line 205).
   - `tui.rs` tests: add `, 80` to the 9 existing `markdown_lines(...)` calls (lines 709-775) and the ones added in Task 1.2.

1. Enable the extension and add table state (top of `markdown_lines`):

```rust
    let parser = Parser::new_ext(text, pulldown_cmark::Options::ENABLE_TABLES);
    // ... existing state ...
    let mut table: Option<TableState> = None;
```

with

```rust
/// Accumulates raw cell text for a markdown table until `TagEnd::Table`.
/// Inline styling inside cells is flattened to plain text.
#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_cell: bool,
}
```

2. Handle the tags. In the `MdEvent::Start` match:

```rust
                Tag::Table(_) => {
                    flush_pending(
                        &mut lines,
                        &mut current_spans,
                        &mut pending,
                        style,
                        &active_prefix(&list_stack, quote_depth, in_heading),
                        in_heading,
                        theme,
                    );
                    table = Some(TableState::default());
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(t) = table.as_mut() {
                        t.current_row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = table.as_mut() {
                        t.current_cell.clear();
                        t.in_cell = true;
                    }
                }
```

In the `MdEvent::End` match:

```rust
                TagEnd::TableCell => {
                    if let Some(t) = table.as_mut() {
                        t.current_row.push(t.current_cell.trim().to_string());
                        t.in_cell = false;
                    }
                }
                TagEnd::TableRow | TagEnd::TableHead => {
                    if let Some(t) = table.as_mut() {
                        if !t.current_row.is_empty() {
                            t.rows.push(std::mem::take(&mut t.current_row));
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = table.take() {
                        render_table(&mut lines, &t.rows, theme);
                    }
                }
```

In `MdEvent::Text` (line 201), route cell text into the table buffer:

```rust
            MdEvent::Text(content) => {
                if in_code_block {
                    code_buffer.push_str(&content);
                } else if table.as_ref().is_some_and(|t| t.in_cell) {
                    if let Some(t) = table.as_mut() {
                        t.current_cell.push_str(&content);
                    }
                } else {
                    pending.push_str(&content);
                }
            }
```

Do the same for `MdEvent::Code` when `in_cell` (append the raw content to `current_cell` instead of pushing a styled span).

3. Add the renderer at the bottom of `markdown.rs`:

```rust
/// Render collected table rows with ` │ ` column separators, padding every
/// cell to its column's display width so columns align. A `─` separator
/// line follows the first (header) row.
fn render_table(lines: &mut Vec<Line<'static>>, rows: &[Vec<String>], theme: &Theme) {
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().map(char_width).sum::<usize>());
        }
    }
    let cell_width = |cell: &str| cell.chars().map(char_width).sum::<usize>();
    for (row_idx, row) in rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, cell) in row.iter().enumerate() {
            let pad = widths[i].saturating_sub(cell_width(cell));
            spans.push(Span::raw(format!("{cell}{}", " ".repeat(pad))));
            if i + 1 < row.len() {
                spans.push(Span::styled(" │ ", Style::default().fg(theme.tool_bar)));
            }
        }
        lines.push(Line::from(spans));
        if row_idx == 0 && rows.len() > 1 {
            let sep = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─");
            lines.push(Line::from(Span::styled(
                sep,
                Style::default().fg(theme.tool_bar),
            )));
        }
    }
}
```

Note: `flush_pending` gained a `theme` parameter in Task 1.2; the `Tag::Table` call site above shows it. `char_width` is already imported in `markdown.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Phase 3 final gate + commit (commit only if confirmed)**

```bash
cargo build --workspace --all-targets && cargo clippy --workspace --all-targets && cargo test --workspace --all-targets && cargo fmt -- --check
git add crates/legion-cli/src/
git commit -m "feat(tui): render markdown tables with aligned columns"
```

---

## Phase 4 — Status surface & docs consistency

### Task 4.1: `/status` shows UI state; refresh stale gap docs

**Files:**
- Modify: `crates/legion-cli/src/slash_commands.rs:367-381`
- Modify: `docs/design/gaps/05-grok-cli-comparison.md` (lines ~97-101 and ~356 — read the surrounding context before editing)
- Modify: `docs/DEVLOG.md` (append an entry, matching existing format)

**Interfaces:**
- Consumes: `AppState.theme_name` (Task 1.3), `ScreenMode::name` (Task 1.3).

- [ ] **Step 1: Write the failing test** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
#[test]
fn status_command_reports_theme_and_viewport() {
    let mut state = AppState::default();
    state.status = "local".to_string();
    state.session_peer = "peer123".to_string();
    state.theme_name = "light".to_string();
    let result = crate::slash_commands::dispatch(&mut state, "/status");
    assert!(matches!(result, crate::slash_commands::CommandResult::Handled));
    let last = state.messages().last().expect("status message");
    assert!(last.content.contains("theme: light"));
    assert!(last.content.contains("viewport: fullscreen"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p legion-cli status_command_reports`
Expected: FAIL — current `/status` has no theme/viewport lines.

- [ ] **Step 3: Implement**

`cmd_status`:

```rust
fn cmd_status(state: &mut AppState, _args: &str) -> CommandResult {
    let peer = if state.session_peer.is_empty() {
        "(unknown)".to_string()
    } else {
        state.session_peer.clone()
    };
    state.push_message(
        MessageRole::System,
        format!(
            "status: {}\nsession: {peer} (resume with `legion --session {peer}`)\ntheme: {} · viewport: {}",
            state.status,
            state.theme_name,
            state.screen_mode.name()
        ),
    );
    CommandResult::Handled
}
```

Docs updates:
- `docs/design/gaps/05-grok-cli-comparison.md`: remove/revise the stale "Legion 缺少 /theme / 无主题系统" claims (already closed by T1) and the "配置无 TUI 主题、screen mode 等 UI 相关段" claim (closed by Task 1.3). Replace with the remaining gaps: cancel unsupported in gateway mode, setup wizard does not share the TUI theme, themes limited to dark/light (no user-defined themes, no terminal theme detection).
- `docs/DEVLOG.md`: append a dated entry summarizing this overhaul (theme persistence, cancel/queue/newline, spinner/indicators, OSC 8 + code block + paste fixes), matching the existing entry format.

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/ docs/
git commit -m "feat(tui): /status reports theme and viewport; refresh gap docs"
```

---

## Phase 5 — Bug fixes

### Task 5.1: Escape-aware width measurement and wrapping (OSC 8 fix)

**Files:**
- Modify: `crates/legion-cli/src/tui/input.rs:22-24` area (new helpers)
- Modify: `crates/legion-cli/src/tui/render.rs:17-89` (`wrap_line_to_width`)

**Interfaces:**
- Produces: `input::visible_width(s: &str) -> usize` (display width ignoring ANSI/OSC escape sequences); `input::next_display_unit(s: &str) -> (&str, &str)` (splits off the next unit: an entire escape sequence, or one char).

- [ ] **Step 1: Write the failing tests** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
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
    let line = Line::from(Span::raw(link.clone()));
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p legion-cli visible_width wrap_does_not_split`
Expected: FAIL — escape bytes are counted as width today, so `visible_width` does not exist and the wrap test would miscount.

- [ ] **Step 3: Implement**

1. `input.rs` — add after `char_width`:

```rust
/// Display width of `s`, ignoring ANSI escape sequences: CSI (`ESC [ … final`)
/// and OSC (`ESC ] … BEL` or `ESC ] … ESC \`), which take no terminal cells.
/// This matters for OSC 8 hyperlinks, whose bytes live inside span content.
pub(crate) fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let (unit, tail) = next_display_unit(rest);
        if !unit.starts_with('\x1b') {
            width += unit.chars().map(char_width).sum::<usize>();
        }
        rest = tail;
    }
    width
}

/// Split `s` into its first display unit and the remainder. A unit is either
/// a full escape sequence (CSI or OSC) or a single char, so wrapping never
/// tears a sequence apart.
pub(crate) fn next_display_unit(s: &str) -> (&str, &str) {
    if s.starts_with('\x1b') {
        let end = escape_sequence_len(s);
        s.split_at(end)
    } else {
        let c = s.chars().next().expect("non-empty input");
        s.split_at(c.len_utf8())
    }
}

/// Byte length of the escape sequence at the start of `s` (which begins with
/// ESC). Unterminated sequences consume the rest of the string.
fn escape_sequence_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    match bytes.get(1) {
        Some(b'[') => {
            // CSI: parameters/intermediates, then a final byte in 0x40..=0x7E.
            let mut i = 2;
            while i < bytes.len() {
                if (0x40..=0x7e).contains(&bytes[i]) {
                    return i + 1;
                }
                i += 1;
            }
            bytes.len()
        }
        Some(b']') => {
            // OSC: ends at BEL or ST (ESC \).
            let mut i = 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            bytes.len()
        }
        _ => 1,
    }
}
```

2. `render.rs` `wrap_line_to_width`:
   - Replace the local `str_width` closure (line 23) with `input::visible_width`:

```rust
    let str_width = |s: &str| crate::tui::input::visible_width(s);
```

   (`char_width` is still used elsewhere in `render.rs` — check imports and remove it only if unused after this change.)
   - Replace the char-by-char split loop (lines 61-72) with unit-based splitting:

```rust
            // Span is wider than the viewport: split it into display units.
            // Escape sequences (OSC 8 links) move as atomic zero-width units
            // so a sequence is never torn across lines.
            let span_style = span.style;
            let mut piece = String::new();
            let mut piece_width = 0usize;
            let mut rest = span.content.as_ref();
            while !rest.is_empty() {
                let (unit, tail) = crate::tui::input::next_display_unit(rest);
                rest = tail;
                let cw = if unit.starts_with('\x1b') {
                    0
                } else {
                    unit.chars().map(char_width).sum::<usize>()
                };
                if piece_width + cw > width && !piece.is_empty() {
                    result.push(
                        Line::from(vec![Span::styled(std::mem::take(&mut piece), span_style)])
                            .style(line_style),
                    );
                    piece_width = 0;
                }
                piece.push_str(unit);
                piece_width += cw;
            }
            if !piece.is_empty() {
                current_spans.push(Span::styled(piece, span_style));
                current_width = piece_width;
            }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "fix(tui): measure and wrap OSC 8 hyperlinks by display width"
```

### Task 5.2: Code blocks fill the viewport width with a uniform background

**Files:**
- Modify: `crates/legion-cli/src/tui/markdown.rs:23-28` (signature), `:353-413` (`emit_code_block`)
- Modify: `crates/legion-cli/src/tui/widgets.rs:156-163, 201-206` (pass width through)
- Modify: `crates/legion-cli/src/tui.rs` tests calling `markdown_lines` (9 call sites, lines 709-775) — add the width argument (`80`)

**Interfaces:**
- Changes: `emit_code_block(lines, buffer, lang, theme, highlighter, viewport_width: u16)` gains the width parameter; `markdown_lines` already has `viewport_width` from Task 3.4 — rename `_viewport_width` to `viewport_width` in both `markdown.rs` and `widgets.rs` (`message_lines`) and thread it into both `emit_code_block` call sites (lines 126 and 255).

- [ ] **Step 1: Write the failing test** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
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
    assert!(lines.len() >= 4, "border + 2 code lines expected");
    let widths: Vec<usize> = lines.iter().map(|l| visible_width(&l.to_string())).collect();
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p legion-cli code_block_lines_have_uniform`
Expected: FAIL — short code lines currently end at their last content span.

- [ ] **Step 3: Implement**

1. `markdown.rs` — rename `_viewport_width` to `viewport_width` in `markdown_lines` and pass it to both `emit_code_block` call sites (lines 126 and 255). In `widgets.rs` `message_lines`, rename `_viewport_width` to `viewport_width` as well (it is already passed to `markdown_lines` since Task 3.4).

3. `emit_code_block` — new signature and padded output:

```rust
pub(crate) fn emit_code_block(
    lines: &mut Vec<Line<'static>>,
    buffer: &mut String,
    lang: &mut String,
    theme: &Theme,
    highlighter: &Highlighter,
    viewport_width: u16,
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
    // Fill the viewport (capped below at 24 columns) so the block background
    // is uniform; over-wide content still wraps via `wrap_line_to_width`.
    let block_width = (max_content_width + line_num_width + 3)
        .max(24)
        .min((viewport_width as usize).max(24));

    // Top border with language label.
    let label = if lang.is_empty() { "code" } else { lang.as_str() };
    let label_width = label.chars().count();
    let border_fill = block_width.saturating_sub(label_width + 4);
    lines.push(Line::from(Span::styled(
        format!("─ {} {}─", label, "─".repeat(border_fill)),
        code_style,
    )));

    let highlighted = highlighter.highlight_lines(lang, buffer, theme);

    for (idx, line) in code_lines.iter().enumerate() {
        let num = idx + 1;
        let gutter = format!("{:>width$} │ ", num, width = line_num_width);
        let mut spans = vec![Span::styled(gutter.clone(), gutter_style)];
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
        // Pad to the block width so every row's background reaches the same
        // right edge.
        let content_width: usize = line.chars().map(char_width).sum();
        let used = gutter.chars().count() + content_width;
        if used < block_width {
            spans.push(Span::styled(" ".repeat(block_width - used), code_style));
        }
        lines.push(Line::from(spans));
    }
    buffer.clear();
    lang.clear();
}
```

4. `tui.rs` tests: the `markdown_lines(...)` call sites already carry the `80` argument (updated in Task 3.4); verify nothing references the old 3-argument signature.

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "fix(tui): pad code blocks to a uniform viewport width background"
```

### Task 5.3: Keep paste placeholders expandable from history

**Files:**
- Modify: `crates/legion-cli/src/input.rs` — actually `crates/legion-cli/src/tui/input.rs:83-96` (`commit_and_clear_input`)

**Interfaces:**
- Consumes: nothing. Behavior change: `paste_store` survives commits.

- [ ] **Step 1: Write the failing test** (append to `crates/legion-cli/src/tui.rs` test module)

```rust
#[test]
fn recalled_history_entry_still_expands_paste_placeholder() {
    let mut state = AppState::default();
    let big = "x".repeat(2000); // exceeds PASTE_CHAR_THRESHOLD
    crate::tui::input::handle_paste(&mut state, big.clone());
    let placeholder = state.composer.join();
    assert!(placeholder.contains("Pasted text"));
    // Send it, then recall it from history.
    crate::tui::input::commit_and_clear_input(&mut state, &placeholder);
    crate::tui::input::navigate_input_history(&mut state, true);
    let expanded = crate::tui::input::expand_paste_placeholders(
        &state.composer.join(),
        &state.paste_store,
    );
    assert_eq!(expanded, big);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p legion-cli recalled_history_entry`
Expected: FAIL — `commit_and_clear_input` clears `paste_store`, so the recalled placeholder expands to itself/empty.

- [ ] **Step 3: Implement**

`tui/input.rs` `commit_and_clear_input`: delete the line `state.paste_store.clear();` (line 95) and adjust the doc comment:

```rust
/// Record `text` in the session input history and clear the input box,
/// resetting all transient input state. `paste_store` is deliberately kept:
/// history entries contain paste placeholders, and recalling one with ↑
/// must still expand to the original pasted text. Placeholder ids are
/// unique per session, so retention cannot cross-expand.
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Commit (only if confirmed)**

```bash
git add crates/legion-cli/src/
git commit -m "fix(tui): keep paste store so history-recalled placeholders expand"
```

### Task 5.4: Inline mode renders tool cards as plain text, not raw JSON

**Files:**
- Modify: `crates/legion-cli/src/tui/inline.rs:15-30`

**Interfaces:**
- Consumes: `tool_card::parse_tool_card`, `tool_card::truncate_chars`, `state::TOOL_ARGS_MAX_CHARS`.

- [ ] **Step 1: Write the failing test** (append to the `inline.rs` test module)

```rust
#[test]
fn tool_message_emits_formatted_card_not_json() {
    let (mut state, mut captured) = state_with_messages(ScreenMode::Inline);
    state.messages.push(ChatMessage::new(
        MessageRole::Tool,
        crate::tui::tool_card::tool_card_json("done", "exec", Some("{\"cmd\":\"ls\"}"), Some("file1\nfile2")),
    ));
    emit_finalized_messages(&mut state, |bytes| {
        captured.extend_from_slice(&bytes);
        Ok(())
    })
    .unwrap();
    let text = String::from_utf8(captured).unwrap();
    assert!(text.contains("▸ exec · done"), "card header missing: {text}");
    assert!(text.contains("file1"), "result body missing: {text}");
    assert!(!text.contains("\"state\""), "raw JSON leaked: {text}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p legion-cli tool_message_emits`
Expected: FAIL — today the raw JSON payload is dumped.

- [ ] **Step 3: Implement** (`inline.rs`)

```rust
use crate::tui::state::{
    AppState, ChatMessage, MessageRole, MessageState, ScreenMode, TOOL_ARGS_MAX_CHARS,
};
use crate::tui::tool_card::{parse_tool_card, truncate_chars};

/// Render a tool card as plain scrollback text (inline mode has no styling).
fn tool_card_to_scrollback(content: &str) -> String {
    let (state, name, arguments, result) = parse_tool_card(content);
    let mut out = format!("\n▸ {name} · {state}\n");
    if let Some(args) = arguments {
        out.push_str(&format!("│ args: {}\n", truncate_chars(&args, TOOL_ARGS_MAX_CHARS)));
    }
    if let Some(res) = result {
        for line in res.lines().take(20) {
            out.push_str(&format!("│ {line}\n"));
        }
        if res.lines().count() > 20 {
            out.push_str("│ …\n");
        }
    }
    out
}
```

and change the `MessageRole::Tool` arm of `message_to_scrollback`:

```rust
        MessageRole::Tool => tool_card_to_scrollback(&msg.content),
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p legion-cli && cargo clippy -p legion-cli --all-targets`
Expected: PASS.

- [ ] **Step 5: Phase 5 + full-project final gate + commit (commit only if confirmed)**

```bash
cargo build --workspace --all-targets && cargo clippy --workspace --all-targets && cargo test --workspace --all-targets && cargo fmt -- --check
git add crates/legion-cli/src/
git commit -m "fix(tui): render tool cards as plain text in inline mode"
```

---

## Out of scope (explicitly deferred)

- **Dedicated header area** (model name / workspace / token usage): the data is not currently plumbed into `AppState`, and the ranked items it would displace are covered by the chat-frame indicators (Task 3.2) and `/status` (Task 4.1). Revisit when model/token telemetry reaches the TUI.
- **Gateway-mode cancel** (`agent.cancel` RPC): needs a cross-crate gateway protocol change. WsDriver returns a clear error message instead (Task 2.1).
- **Setup wizard restyle/theming**: `setup.rs` is a separate line-oriented UI (no ratatui); unifying it with `Theme` is a standalone project. Noted in the refreshed gap docs (Task 4.1).
- **Render latency** (replacing `crossterm::event::poll` with `EventStream` + `tokio::select!`): a structural change to `tui_loop`; the 100ms ceiling remains.
- **Mouse cursor placement in the composer / double-click selection**: `Composer::handle_mouse` stays a documented no-op.
- **Splitting `tui.rs` (2372 lines)**: file-size hygiene noted in the analysis; not part of these five workstreams.
