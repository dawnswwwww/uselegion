//! Interactive setup wizard for first-time Legion configuration.

use crate::CliError;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::Path;

/// Options that can be provided to skip interactive prompts.
#[derive(Debug, Default, Clone)]
pub struct SetupOptions {
    /// Provider preset key (see [`presets`]). Defaults to `minimax` when
    /// `api_key` is given without a provider, for backward compatibility.
    pub provider: Option<String>,
    /// API key for the selected provider. May be a literal value or an
    /// environment variable reference such as `${OPENAI_API_KEY}`.
    pub api_key: Option<String>,
    /// Default model override for the selected provider.
    pub model: Option<String>,
    /// Base URL override (required for the `custom` preset).
    pub base_url: Option<String>,
    pub gateway_token: Option<String>,
    pub bind_host: Option<String>,
    pub port: Option<u16>,
    /// Overwrite an existing configuration (a `.bak` backup is written).
    pub force: bool,
    /// Merge the selected provider into an existing configuration instead of
    /// rewriting it (interactive runs offer this as a menu choice).
    pub add_provider: bool,
    /// Install the gateway as a system service (launchd / systemd user unit /
    /// Windows logon task).
    pub install_daemon: bool,
}

/// How a provider preset authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// A single API key; `env_var` is the conventional environment variable
    /// that is detected and offered as a `${VAR}` reference during setup.
    ApiKey { env_var: &'static str },
    /// AWS SigV4 credentials (access key / secret key / region).
    AwsSigv4,
    /// No credentials required (e.g. a local Ollama).
    None,
}

/// A built-in provider preset offered by the setup wizard.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    /// Key used on the CLI (`--provider <key>`) and as the model alias.
    pub key: &'static str,
    /// Human-readable label shown in the interactive menu.
    pub label: &'static str,
    /// Provider id written to `models.providers`.
    pub provider_id: &'static str,
    /// `kind` understood by the provider router.
    pub kind: &'static str,
    /// Base URL written to the provider config; `None` uses the
    /// provider implementation's built-in default.
    pub base_url: Option<&'static str>,
    /// Default model written to the provider config and alias.
    pub default_model: &'static str,
    pub auth: AuthKind,
}

/// Key of the pseudo-preset for a fully custom OpenAI-compatible endpoint.
pub const CUSTOM_PRESET_KEY: &str = "custom";

/// Built-in provider presets, in menu order.
pub fn presets() -> &'static [ProviderPreset] {
    &[
        ProviderPreset {
            key: "minimax",
            label: "MiniMax (api.minimaxi.com)",
            provider_id: "minimax-openai",
            kind: "openai",
            base_url: Some("https://api.minimaxi.com/v1"),
            default_model: "MiniMax-M3",
            auth: AuthKind::ApiKey {
                env_var: "MINIMAX_API_KEY",
            },
        },
        ProviderPreset {
            key: "openai",
            label: "OpenAI",
            provider_id: "openai",
            kind: "openai",
            base_url: None,
            default_model: "gpt-4o",
            auth: AuthKind::ApiKey {
                env_var: "OPENAI_API_KEY",
            },
        },
        ProviderPreset {
            key: "anthropic",
            label: "Anthropic",
            provider_id: "anthropic",
            kind: "anthropic",
            base_url: None,
            default_model: "claude-sonnet-4-5",
            auth: AuthKind::ApiKey {
                env_var: "ANTHROPIC_API_KEY",
            },
        },
        ProviderPreset {
            key: "gemini",
            label: "Google Gemini",
            provider_id: "gemini",
            kind: "gemini",
            base_url: None,
            default_model: "gemini-2.5-flash",
            auth: AuthKind::ApiKey {
                env_var: "GEMINI_API_KEY",
            },
        },
        ProviderPreset {
            key: "ollama",
            label: "Ollama (local, no API key)",
            provider_id: "ollama",
            kind: "ollama",
            base_url: None,
            default_model: "llama3.1",
            auth: AuthKind::None,
        },
        ProviderPreset {
            key: "openrouter",
            label: "OpenRouter",
            provider_id: "openrouter",
            kind: "openrouter",
            base_url: Some("https://openrouter.ai/api/v1"),
            default_model: "openai/gpt-4o",
            auth: AuthKind::ApiKey {
                env_var: "OPENROUTER_API_KEY",
            },
        },
        ProviderPreset {
            key: "bedrock",
            label: "AWS Bedrock (SigV4)",
            provider_id: "bedrock",
            kind: "bedrock",
            base_url: None,
            default_model: "anthropic.claude-sonnet-4-5",
            auth: AuthKind::AwsSigv4,
        },
    ]
}

/// Look up a preset by its key.
pub fn preset_by_key(key: &str) -> Option<&'static ProviderPreset> {
    presets().iter().find(|p| p.key == key)
}

/// Default gateway token length in bytes.
const TOKEN_BYTES: usize = 32;

/// Generate a random URL-safe token.
pub fn generate_token() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    let mut rng = rand::rng();
    (0..TOKEN_BYTES)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

/// Prompt the user on stdin for a value.
pub fn prompt(message: &str) -> Result<String, CliError> {
    print!("{} ", message);
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Prompt for a value, falling back to `default` when the user enters nothing.
pub fn prompt_default(message: &str, default: &str) -> Result<String, CliError> {
    let answer = prompt(&format!("{message} [{default}]:"))?;
    if answer.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(answer)
    }
}

/// Layout of a [`select`] prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectLayout {
    /// One option per line; move with ↑/↓.
    Vertical,
    /// All options on one line; move with ←/→.
    Horizontal,
}

/// Prompt for a yes/no answer; `default_yes` controls the initial highlight.
///
/// Rendered as a horizontal ←/→ selector ([`select`]); `y`/`n` also work.
pub fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool, CliError> {
    let default = if default_yes { 0 } else { 1 };
    Ok(select(message, &["Yes", "No"], default, SelectLayout::Horizontal)? == 0)
}

/// Arrow-key selection among `options`, returning the chosen index.
///
/// Vertical lists move with ↑/↓, horizontal groups with ←/→. Digit keys
/// (1-9) and the first letter of an option choose it immediately; Enter
/// confirms; Esc/Ctrl-C cancels. Falls back to a typed answer when stdin is
/// not a terminal or raw mode is unavailable.
pub fn select(
    message: &str,
    options: &[&str],
    default: usize,
    layout: SelectLayout,
) -> Result<usize, CliError> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use crossterm::{cursor, execute, terminal};
    use std::io::IsTerminal;

    assert!(!options.is_empty(), "select needs at least one option");
    let mut selected = default.min(options.len() - 1);

    if !std::io::stdin().is_terminal() || enable_raw_mode().is_err() {
        return select_fallback(message, options, selected, layout);
    }

    let mut stdout = std::io::stdout();
    let mut lines = render_select(&mut stdout, message, options, selected, layout)?;

    let result = (|| -> Result<usize, CliError> {
        loop {
            let chose = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter => Some(selected),
                    KeyCode::Esc => {
                        return Err(CliError::Cancelled);
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(CliError::Cancelled);
                    }
                    KeyCode::Up if layout == SelectLayout::Vertical => {
                        selected = selected.saturating_sub(1);
                        None
                    }
                    KeyCode::Down if layout == SelectLayout::Vertical => {
                        selected = (selected + 1).min(options.len() - 1);
                        None
                    }
                    KeyCode::Left if layout == SelectLayout::Horizontal => {
                        selected = selected.saturating_sub(1);
                        None
                    }
                    KeyCode::Right if layout == SelectLayout::Horizontal => {
                        selected = (selected + 1).min(options.len() - 1);
                        None
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let number = c.to_digit(10).expect("ascii digit") as usize;
                        if (1..=options.len()).contains(&number) {
                            selected = number - 1;
                            Some(selected)
                        } else {
                            None
                        }
                    }
                    KeyCode::Char(c) => options
                        .iter()
                        .position(|o| o.to_ascii_lowercase().starts_with(c.to_ascii_lowercase()))
                        .map(|index| {
                            selected = index;
                            selected
                        }),
                    _ => None,
                },
                _ => None,
            };
            if let Some(chosen) = chose {
                break Ok(chosen);
            }
            execute!(
                stdout,
                cursor::MoveUp(lines as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )?;
            lines = render_select(&mut stdout, message, options, selected, layout)?;
        }
    })();

    let _ = disable_raw_mode();

    // Collapse the menu into a single scrollback line showing the choice.
    execute!(
        stdout,
        cursor::MoveUp(lines as u16),
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    )?;
    match &result {
        Ok(chosen) => {
            use crossterm::style::Stylize;
            writeln!(stdout, "{message} {}", options[*chosen].bold())?;
        }
        Err(_) => {
            println!();
        }
    }
    stdout.flush()?;
    result
}

/// Render the select menu; returns the number of lines printed.
///
/// Lines are terminated with `\r\n`: raw mode disables output post-processing,
/// so a bare `\n` would move the cursor down without returning to column 0
/// and render the menu as a staircase. Lines are also truncated to the
/// terminal width — a wrapped row would occupy two screen rows and break the
/// move-up-and-redraw math.
fn render_select(
    stdout: &mut dyn Write,
    message: &str,
    options: &[&str],
    selected: usize,
    layout: SelectLayout,
) -> std::io::Result<usize> {
    use crossterm::style::Stylize;

    let width = crossterm::terminal::size()
        .map(|(w, _)| (w as usize).max(20))
        .unwrap_or(120);

    write!(stdout, "{}\r\n", fit_width(message, width))?;
    let hint = match layout {
        SelectLayout::Vertical => "(↑/↓ move, Enter select, number or first letter jumps)",
        SelectLayout::Horizontal => "(←/→ move, Enter select)",
    };
    write!(stdout, "{}\r\n", fit_width(hint, width).dim())?;
    let mut lines = 2;
    match layout {
        SelectLayout::Vertical => {
            // 2 columns are reserved for the pointer / indent.
            let budget = width.saturating_sub(2).max(4);
            for (index, option) in options.iter().enumerate() {
                let label = fit_width(option, budget);
                if index == selected {
                    write!(stdout, "{} {}\r\n", "❯".green(), label.bold())?;
                } else {
                    write!(stdout, "  {label}\r\n")?;
                }
                lines += 1;
            }
        }
        SelectLayout::Horizontal => {
            // Each segment costs label + 4 ("  x  " / "[ x ]") plus a
            // 3-column gap between options; share the remaining width evenly.
            let count = options.len();
            let per_option = (width.saturating_sub(3 * count.saturating_sub(1)) / count)
                .saturating_sub(4)
                .max(4);
            for (index, option) in options.iter().enumerate() {
                let label = fit_width(option, per_option);
                if index > 0 {
                    write!(stdout, "   ")?;
                }
                if index == selected {
                    write!(stdout, "{}", format!("[ {label} ]").reverse())?;
                } else {
                    write!(stdout, "  {label}  ")?;
                }
            }
            write!(stdout, "\r\n")?;
            lines += 1;
        }
    }
    stdout.flush()?;
    Ok(lines)
}

/// Truncate `text` to at most `max` characters, appending an ellipsis when
/// truncated. Apply this to plain text *before* adding ANSI styling.
fn fit_width(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
        truncated.push('…');
        truncated
    }
}

/// Text-input fallback for [`select`] when the terminal is not interactive.
fn select_fallback(
    message: &str,
    options: &[&str],
    default: usize,
    layout: SelectLayout,
) -> Result<usize, CliError> {
    println!("{message}");
    match layout {
        SelectLayout::Vertical => {
            for (index, option) in options.iter().enumerate() {
                println!("  {}) {}", index + 1, option);
            }
        }
        SelectLayout::Horizontal => {
            println!("  {}", options.join(" / "));
        }
    }
    loop {
        let answer = prompt(&format!("Choose 1-{} [{}]:", options.len(), default + 1))?;
        if answer.is_empty() {
            return Ok(default);
        }
        if let Ok(number) = answer.parse::<usize>() {
            if (1..=options.len()).contains(&number) {
                return Ok(number - 1);
            }
        }
        let lower = answer.to_lowercase();
        if let Some(index) = options
            .iter()
            .position(|o| o.to_lowercase().starts_with(&lower))
        {
            return Ok(index);
        }
        println!("Please enter a number between 1 and {}.", options.len());
    }
}

/// Prompt for a secret, echoing `*` instead of the typed characters.
///
/// Falls back to a plain (echoed) prompt when the terminal does not support
/// raw mode.
pub fn prompt_secret(message: &str) -> Result<String, CliError> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use std::io::IsTerminal;

    print!("{} ", message);
    std::io::stdout().flush()?;

    if !std::io::stdin().is_terminal() || enable_raw_mode().is_err() {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        return Ok(input.trim().to_string());
    }

    let mut value = String::new();
    let result = (|| -> Result<String, CliError> {
        loop {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter => break,
                    KeyCode::Esc => {
                        return Err(CliError::Cancelled);
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(CliError::Cancelled);
                    }
                    KeyCode::Char(c) => {
                        value.push(c);
                        print!("*");
                        std::io::stdout().flush()?;
                    }
                    KeyCode::Backspace if value.pop().is_some() => {
                        print!("\x08 \x08");
                        std::io::stdout().flush()?;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        // Raw mode disables output post-processing, so emit an explicit
        // carriage return to land at the start of the next line.
        print!("\r\n");
        std::io::stdout().flush()?;
        Ok(value.trim().to_string())
    })();
    let _ = disable_raw_mode();
    result
}

/// Fully resolved setup choices, independent of how they were gathered
/// (interactive prompts or CLI flags).
#[derive(Debug, Clone)]
pub struct SetupChoices {
    pub provider_id: String,
    pub alias: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub default_model: String,
    pub auth_profile_name: String,
    /// The auth profile value, or `None` when the provider needs no
    /// credentials (e.g. Ollama).
    pub auth_profile: Option<Value>,
}

impl SetupChoices {
    /// Resolve non-interactive choices from CLI options.
    pub fn from_options(opts: &SetupOptions) -> Result<Self, CliError> {
        let provider_key = opts
            .provider
            .clone()
            .or_else(|| opts.api_key.as_ref().map(|_| "minimax".to_string()))
            .ok_or_else(|| {
                CliError::Other(format!(
                    "--provider is required in non-interactive mode (one of: {}, {CUSTOM_PRESET_KEY})",
                    presets()
                        .iter()
                        .map(|p| p.key)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        if provider_key == CUSTOM_PRESET_KEY {
            let base_url = required_opt(
                &opts.base_url,
                "--base-url is required for --provider custom",
            )?;
            let model = required_opt(&opts.model, "--model is required for --provider custom")?;
            let key = required_opt(&opts.api_key, "--api-key is required for --provider custom")?;
            return Ok(Self {
                provider_id: "custom".to_string(),
                alias: "custom".to_string(),
                kind: "openai".to_string(),
                base_url: Some(base_url),
                default_model: model,
                auth_profile_name: "custom-default".to_string(),
                auth_profile: Some(json!({ "type": "api_key", "key": key })),
            });
        }

        let preset = preset_by_key(&provider_key).ok_or_else(|| {
            CliError::Other(format!(
                "unknown provider '{provider_key}' (one of: {}, {CUSTOM_PRESET_KEY})",
                presets()
                    .iter()
                    .map(|p| p.key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

        let auth_profile = match preset.auth {
            AuthKind::ApiKey { env_var } => {
                let key = required_opt(&opts.api_key, "--api-key is required for this provider")?;
                if is_placeholder_key(&key) {
                    return Err(CliError::Other(format!(
                        "--api-key looks like a placeholder; provide a real key or set ${env_var}"
                    )));
                }
                Some(json!({ "type": "api_key", "key": key }))
            }
            AuthKind::AwsSigv4 => {
                return Err(CliError::Other(
                    "the bedrock provider needs access key, secret key and region; \
                     run `legion setup` interactively"
                        .to_string(),
                ));
            }
            AuthKind::None => None,
        };

        Ok(Self::from_preset(
            preset,
            opts.model.clone(),
            opts.base_url.clone(),
            auth_profile,
        ))
    }

    fn from_preset(
        preset: &ProviderPreset,
        model: Option<String>,
        base_url: Option<String>,
        auth_profile: Option<Value>,
    ) -> Self {
        Self {
            provider_id: preset.provider_id.to_string(),
            alias: preset.key.to_string(),
            kind: preset.kind.to_string(),
            base_url: base_url.or_else(|| preset.base_url.map(str::to_string)),
            default_model: model.unwrap_or_else(|| preset.default_model.to_string()),
            auth_profile_name: format!("{}-default", preset.key),
            auth_profile,
        }
    }
}

fn required_opt(value: &Option<String>, message: &str) -> Result<String, CliError> {
    value
        .clone()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| CliError::Other(message.to_string()))
}

/// Build the `models.providers.<id>` entry for the resolved choices.
pub fn provider_config_json(choices: &SetupChoices) -> Value {
    let mut provider = json!({
        "id": choices.provider_id,
        "kind": choices.kind,
        "authProfile": choices.auth_profile_name,
        "defaultModel": choices.default_model,
    });
    if let Some(base_url) = &choices.base_url {
        provider["baseUrl"] = json!(base_url);
    }
    provider
}

/// Build the Legion config JSON string from setup choices.
pub fn build_config_json(
    choices: &SetupChoices,
    opts: &SetupOptions,
    channels: &[(String, Value)],
) -> String {
    let token = opts.gateway_token.clone().unwrap_or_else(generate_token);
    let bind_host = opts
        .bind_host
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = opts.port.unwrap_or(18789);

    let mut config = json!({
        "gateway": {
            "bindHost": bind_host,
            "port": port,
            "auth": { "mode": "token", "token": token }
        },
        "agents": {
            "defaults": {
                "model": choices.alias
            }
        },
        "models": {
            "providers": {
                choices.provider_id.clone(): provider_config_json(choices)
            },
            "aliases": {
                choices.alias.clone(): format!("{}/{}", choices.provider_id, choices.default_model)
            }
        }
    });
    if !channels.is_empty() {
        let map: Map<String, Value> = channels.iter().cloned().collect();
        config["channels"] = Value::Object(map);
    }

    serde_json::to_string_pretty(&config).expect("generated config is always valid")
}

/// Build the default Legion config from setup choices.
pub fn build_config(choices: &SetupChoices, opts: &SetupOptions) -> legion_core::config::Config {
    legion_core::config::Config::from_json(&build_config_json(choices, opts, &[]))
        .expect("generated config is always valid")
}

/// Outcome of the advisory live connection test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    /// The provider answered and the credentials appear valid.
    Verified,
    /// The endpoint does not support the lightweight probe (e.g. it has no
    /// model-listing route), so the credentials could not be verified —
    /// this does not mean they are wrong.
    Unverifiable(String),
    /// The probe failed in a way that points at a real problem: bad key,
    /// unreachable host, or a server error.
    Failed(String),
}

/// Try a lightweight live check of the chosen provider credentials.
///
/// The check is advisory: [`TestOutcome::Unverifiable`] means the probe
/// itself is unsupported by the endpoint, not that anything is wrong.
pub async fn test_connection(choices: &SetupChoices) -> TestOutcome {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(e) => return TestOutcome::Failed(format!("failed to build HTTP client: {e}")),
    };

    /// Classify an HTTP status from a model-listing probe.
    fn classify(status: reqwest::StatusCode, provider: &str) -> TestOutcome {
        match status.as_u16() {
            200..=299 => TestOutcome::Verified,
            401 | 403 => TestOutcome::Failed(format!(
                "{provider} rejected the credentials (HTTP {status})"
            )),
            404 | 405 => TestOutcome::Unverifiable(format!(
                "{provider} has no model-listing endpoint (HTTP {status}); the key was not verified"
            )),
            _ => TestOutcome::Failed(format!("{provider} returned HTTP {status}")),
        }
    }

    let base = choices.base_url.clone();
    match choices.kind.as_str() {
        "ollama" => {
            let base = base.unwrap_or_else(|| "http://localhost:11434".to_string());
            match client.get(format!("{base}/api/tags")).send().await {
                Ok(response) => classify(response.status(), "Ollama"),
                Err(e) => TestOutcome::Failed(format!("cannot reach Ollama at {base}: {e}")),
            }
        }
        "gemini" => {
            let key = match resolve_key(choices) {
                Ok(key) => key,
                Err(reason) => return TestOutcome::Failed(reason),
            };
            let base =
                base.unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_string());
            match client
                .get(format!("{base}/v1beta/models"))
                .query(&[("key", key)])
                .send()
                .await
            {
                Ok(response) => classify(response.status(), "Gemini"),
                Err(e) => TestOutcome::Failed(format!("request failed: {e}")),
            }
        }
        "anthropic" => {
            let key = match resolve_key(choices) {
                Ok(key) => key,
                Err(reason) => return TestOutcome::Failed(reason),
            };
            let base = base.unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());
            match client
                .get(format!("{base}/models"))
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
            {
                Ok(response) => classify(response.status(), "Anthropic"),
                Err(e) => TestOutcome::Failed(format!("request failed: {e}")),
            }
        }
        "bedrock" => TestOutcome::Unverifiable(
            "live testing Bedrock requires SigV4 signing; credentials are verified on first use"
                .to_string(),
        ),
        // openai / generic-openai / openrouter
        _ => {
            let key = match resolve_key(choices) {
                Ok(key) => key,
                Err(reason) => return TestOutcome::Failed(reason),
            };
            let base = base.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            match client
                .get(format!("{base}/models"))
                .bearer_auth(key)
                .send()
                .await
            {
                Ok(response) => classify(response.status(), "the provider"),
                Err(e) => TestOutcome::Failed(format!("request failed: {e}")),
            }
        }
    }
}

/// Resolve the configured API key, expanding a `${VAR}` reference.
fn resolve_key(choices: &SetupChoices) -> Result<String, String> {
    let key = choices
        .auth_profile
        .as_ref()
        .and_then(|p| p.get("key"))
        .and_then(|k| k.as_str())
        .unwrap_or("");
    let trimmed = key.trim();
    if let Some(var) = trimmed
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))
    } else {
        Ok(trimmed.to_string())
    }
}

/// Minimal workspace `AGENTS.md` seeded by setup when none exists.
const WORKSPACE_AGENTS_MD: &str = "# AGENTS.md\n\nWorkspace notes for the Legion agent.\n";

/// Run the interactive or non-interactive setup wizard.
pub async fn run_setup(
    interactive: bool,
    opts: SetupOptions,
    home_dir: &Path,
) -> Result<(), CliError> {
    let config_dir = home_dir.join(".legion");
    let agent_dir = config_dir.join("agents/main/agent");
    let config_path = config_dir.join("legion.json");
    let auth_path = agent_dir.join("auth-profiles.json");

    // When a config already exists, decide whether this run merges a new
    // provider in (add-provider) or rewrites the whole file.
    let add_provider = if config_path.is_file() {
        if opts.add_provider {
            true
        } else if !interactive {
            if !opts.force {
                return Err(CliError::Other(format!(
                    "configuration already exists at {}; pass --force to overwrite \
                     (a .bak backup will be written), or --add-provider to merge \
                     another provider into it",
                    config_path.display()
                )));
            }
            false
        } else {
            println!(
                "An existing Legion configuration was found at {}. Reconfiguring writes a .bak backup first.",
                config_path.display()
            );
            match select(
                "What would you like to do?",
                &["Keep", "Add provider", "Reconfigure", "Abort"],
                0,
                SelectLayout::Horizontal,
            )? {
                0 => {
                    println!("Keeping the existing configuration; nothing changed.");
                    return Ok(());
                }
                1 => true,
                2 => false,
                _ => {
                    return Err(CliError::Other("setup aborted".to_string()));
                }
            }
        }
    } else {
        false
    };

    if add_provider {
        let choices = if interactive {
            gather_interactive(&opts).await?
        } else {
            SetupChoices::from_options(&opts)?
        };
        merge_provider_into_config(&config_path, &choices)?;
        std::fs::create_dir_all(&agent_dir)?;
        if let Some(profile) = &choices.auth_profile {
            merge_auth_profile(&auth_path, &choices.auth_profile_name, profile)?;
        }
        println!();
        println!(
            "Provider '{}' (model: {}) merged into {}",
            choices.provider_id,
            choices.default_model,
            config_path.display()
        );
        if choices.auth_profile.is_some() {
            println!(
                "Auth profile '{}' written to {}",
                choices.auth_profile_name,
                auth_path.display()
            );
        }
        println!(
            "agents.defaults.model was left unchanged; switch it with \
             `legion config set agents.defaults.model {}` if desired.",
            choices.alias
        );
        return Ok(());
    }

    let choices = if interactive {
        gather_interactive(&opts).await?
    } else {
        SetupChoices::from_options(&opts)?
    };

    let mut effective_opts = opts.clone();
    if interactive {
        println!();
        println!("Gateway settings:");
        if effective_opts.bind_host.is_none() {
            let host = prompt_default("Bind host", "127.0.0.1")?;
            effective_opts.bind_host = Some(host);
        }
        if effective_opts.port.is_none() {
            loop {
                let port_str = prompt_default("Port", "18789")?;
                match port_str.parse::<u16>() {
                    Ok(port) => {
                        effective_opts.port = Some(port);
                        break;
                    }
                    Err(_) => println!("'{port_str}' is not a valid port (1-65535)."),
                }
            }
        }
    }
    // The gateway token is always generated unless explicitly provided;
    // users rarely want to invent one themselves.
    let token_was_generated = effective_opts.gateway_token.is_none();

    // Optional channel onboarding (interactive only; non-interactive runs
    // stay provider-only for backward compatibility).
    let channels = if interactive {
        println!();
        gather_channels()?
    } else {
        Vec::new()
    };

    std::fs::create_dir_all(&config_dir)?;
    std::fs::create_dir_all(&agent_dir)?;

    // Back up an existing config before overwriting it.
    if config_path.is_file() {
        let backup_path = config_dir.join("legion.json.bak");
        std::fs::copy(&config_path, &backup_path)?;
        println!("Existing config backed up to {}", backup_path.display());
    }

    let config_text = build_config_json(&choices, &effective_opts, &channels);
    // Validate before writing.
    let _ = legion_core::config::Config::from_json(&config_text)?;
    std::fs::write(&config_path, &config_text)?;

    // Merge the auth profile into any existing auth-profiles.json so other
    // providers' credentials survive a re-run.
    if let Some(profile) = &choices.auth_profile {
        merge_auth_profile(&auth_path, &choices.auth_profile_name, profile)?;
    }

    // Seed the default workspace.
    let workspace_dir = config_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir)?;
    let agents_md = workspace_dir.join("AGENTS.md");
    if !agents_md.exists() {
        std::fs::write(&agents_md, WORKSPACE_AGENTS_MD)?;
    }

    println!();
    println!("Configuration written to {}", config_path.display());
    if choices.auth_profile.is_some() {
        println!(
            "Auth profile '{}' written to {}",
            choices.auth_profile_name,
            auth_path.display()
        );
    }
    println!(
        "Provider: {} (model: {})",
        choices.provider_id, choices.default_model
    );
    if !channels.is_empty() {
        let names: Vec<&str> = channels.iter().map(|(id, _)| id.as_str()).collect();
        println!("Channels: {}", names.join(", "));
    }
    if token_was_generated {
        if let Ok(config) = serde_json::from_str::<Value>(&config_text) {
            if let Some(token) = config
                .get("gateway")
                .and_then(|g| g.get("auth"))
                .and_then(|a| a.get("token"))
                .and_then(|t| t.as_str())
            {
                println!("Gateway token (also stored in the config): {token}");
            }
        }
    }

    let daemon_installed = maybe_install_daemon(interactive, opts.install_daemon, home_dir);

    println!();
    println!("Next steps:");
    if !daemon_installed {
        println!("  legion gateway start     # start the gateway in the background");
    }
    println!("  legion agent \"hello\"     # send a first message to the agent");
    println!("  legion doctor            # run health checks on the installation");

    Ok(())
}

/// Walk the user through provider, credentials and model selection.
async fn gather_interactive(opts: &SetupOptions) -> Result<SetupChoices, CliError> {
    println!("Welcome to the Legion setup wizard.");
    println!();

    let preset = if let Some(key) = &opts.provider {
        if key == CUSTOM_PRESET_KEY {
            None
        } else {
            Some(
                preset_by_key(key)
                    .ok_or_else(|| CliError::Other(format!("unknown provider '{key}'")))?,
            )
        }
    } else {
        let mut labels: Vec<String> = presets().iter().map(|p| p.label.to_string()).collect();
        labels.push("Custom OpenAI-compatible endpoint".to_string());
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let index = select(
            "Select a model provider:",
            &label_refs,
            0,
            SelectLayout::Vertical,
        )?;
        if index == presets().len() {
            None
        } else {
            Some(&presets()[index])
        }
    };

    let Some(preset) = preset else {
        return gather_custom(opts).await;
    };

    let auth_profile = match preset.auth {
        AuthKind::None => None,
        AuthKind::ApiKey { env_var } => {
            let key = gather_api_key(opts, preset.label, env_var)?;
            Some(json!({ "type": "api_key", "key": key }))
        }
        AuthKind::AwsSigv4 => Some(gather_aws_credentials()?),
    };

    let model = if let Some(model) = &opts.model {
        model.clone()
    } else {
        prompt_default("Default model", preset.default_model)?
    };

    let base_url = if preset.key == "ollama" && opts.base_url.is_none() {
        Some(prompt_default("Ollama base URL", "http://localhost:11434")?)
    } else {
        opts.base_url.clone()
    };

    let choices = SetupChoices::from_preset(preset, Some(model), base_url, auth_profile);

    maybe_test_connection(&choices).await?;

    Ok(choices)
}

/// Gather choices for a custom OpenAI-compatible endpoint.
async fn gather_custom(opts: &SetupOptions) -> Result<SetupChoices, CliError> {
    let base_url = match &opts.base_url {
        Some(url) => url.clone(),
        None => loop {
            let url = prompt("Base URL (e.g. https://api.example.com/v1):")?;
            if url.starts_with("http://") || url.starts_with("https://") {
                break url;
            }
            println!("The base URL must start with http:// or https://.");
        },
    };
    let model = match &opts.model {
        Some(model) => model.clone(),
        None => loop {
            let model = prompt("Default model name:")?;
            if !model.trim().is_empty() {
                break model;
            }
            println!("A model name is required.");
        },
    };
    let key = match &opts.api_key {
        Some(key) => key.clone(),
        None => loop {
            let key = prompt_secret("API key:")?;
            if !key.is_empty() {
                break key;
            }
            println!("An API key is required.");
        },
    };

    let choices = SetupChoices {
        provider_id: "custom".to_string(),
        alias: "custom".to_string(),
        kind: "openai".to_string(),
        base_url: Some(base_url),
        default_model: model,
        auth_profile_name: "custom-default".to_string(),
        auth_profile: Some(json!({ "type": "api_key", "key": key })),
    };

    maybe_test_connection(&choices).await?;

    Ok(choices)
}

/// Gather an API key, offering a detected environment variable reference first.
fn gather_api_key(opts: &SetupOptions, label: &str, env_var: &str) -> Result<String, CliError> {
    if let Some(key) = &opts.api_key {
        return Ok(key.clone());
    }

    if let Ok(value) = std::env::var(env_var) {
        if !value.trim().is_empty()
            && prompt_yes_no(
                &format!("${env_var} is set; store a reference to it instead of the raw key?"),
                true,
            )?
        {
            return Ok(format!("${{{env_var}}}"));
        }
    }

    loop {
        let key = prompt_secret(&format!("{label} API key (input is masked):"))?;
        if key.is_empty() {
            println!("An API key is required.");
            continue;
        }
        if is_placeholder_key(&key) {
            println!("That looks like a placeholder; please paste a real API key.");
            continue;
        }
        return Ok(key);
    }
}

/// Gather AWS credentials for the Bedrock provider.
fn gather_aws_credentials() -> Result<Value, CliError> {
    let access_key = match std::env::var("AWS_ACCESS_KEY_ID") {
        Ok(value) if !value.trim().is_empty() => prompt_default("AWS access key ID", &value)?,
        _ => loop {
            let key = prompt("AWS access key ID:")?;
            if !key.is_empty() {
                break key;
            }
            println!("An access key ID is required.");
        },
    };
    let secret_key = loop {
        let key = prompt_secret("AWS secret access key (input is masked):")?;
        if !key.is_empty() {
            break key;
        }
        println!("A secret access key is required.");
    };
    let default_region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let region = prompt_default("AWS region", &default_region)?;

    Ok(json!({
        "type": "aws_sigv4",
        "access_key": access_key,
        "secret_key": secret_key,
        "region": region
    }))
}

/// Chat channels offered by the setup wizard, in menu order.
const CHANNEL_MENU: [(&str, &str); 5] = [
    ("telegram", "Telegram"),
    ("slack", "Slack"),
    ("discord", "Discord"),
    ("lark", "Lark (Feishu)"),
    ("matrix", "Matrix"),
];

/// Build the `channels.<id>.access` value for a DM allowlist.
fn access_json(allowlist: &[String]) -> Value {
    json!({ "dmPolicy": "allowlist", "allowlist": allowlist })
}

/// Attach an access policy when the user supplied allowlist entries.
fn with_access(mut channel: Value, allowlist: &[String]) -> Value {
    if !allowlist.is_empty() {
        channel["access"] = access_json(allowlist);
    }
    channel
}

/// Ask for sender ids allowed to DM the agent on one channel.
///
/// DMs default to the `allowlist` policy, so without at least one entry the
/// bot silently ignores every DM — make that consequence explicit.
fn gather_dm_allowlist(channel_id: &str, id_hint: &str) -> Result<Vec<String>, CliError> {
    let answer = prompt(&format!(
        "Your sender id for the DM allowlist ({id_hint}); comma-separated, empty = configure later:"
    ))?;
    let ids: Vec<String> = answer
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        println!(
            "Note: DMs default to an allowlist, so the bot ignores every DM until you add \
             channels.{channel_id}.access.allowlist (`legion config set`)."
        );
    }
    Ok(ids)
}

/// Prompt for a channel secret, offering a detected environment variable
/// reference first (mirrors provider key handling).
fn gather_channel_secret(label: &str, env_var: &str) -> Result<String, CliError> {
    if let Ok(value) = std::env::var(env_var) {
        if !value.trim().is_empty()
            && prompt_yes_no(
                &format!("${env_var} is set; store a reference to it instead of the raw value?"),
                true,
            )?
        {
            return Ok(format!("${{{env_var}}}"));
        }
    }
    loop {
        let value = prompt_secret(&format!("{label} (input is masked):"))?;
        if value.is_empty() {
            println!("A value is required.");
            continue;
        }
        return Ok(value);
    }
}

/// Walk the optional channel onboarding loop; returns `channels` entries.
fn gather_channels() -> Result<Vec<(String, Value)>, CliError> {
    let mut configured: Vec<(String, Value)> = Vec::new();
    loop {
        let mut labels: Vec<String> = CHANNEL_MENU
            .iter()
            .map(|(id, label)| {
                if configured.iter().any(|(done, _)| done == id) {
                    format!("{label} (configured)")
                } else {
                    label.to_string()
                }
            })
            .collect();
        labels.push("Done".to_string());
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let index = select(
            "Add a chat channel? (WebChat works out of the box)",
            &label_refs,
            label_refs.len() - 1,
            SelectLayout::Vertical,
        )?;
        if index == CHANNEL_MENU.len() {
            break;
        }
        let (id, _) = CHANNEL_MENU[index];
        let channel = match id {
            "telegram" => {
                let token = gather_channel_secret("Telegram bot token", "TELEGRAM_BOT_TOKEN")?;
                let username = prompt("Bot username without @ (optional, Enter to skip):")?;
                let allowlist =
                    gather_dm_allowlist(id, "numeric Telegram user id, e.g. from @userinfobot")?;
                let mut channel = json!({ "token": token });
                if !username.is_empty() {
                    channel["botUsername"] = json!(username.trim_start_matches('@'));
                }
                with_access(channel, &allowlist)
            }
            "slack" => {
                let bot_token =
                    gather_channel_secret("Slack bot token (xoxb-…)", "SLACK_BOT_TOKEN")?;
                let app_token =
                    gather_channel_secret("Slack app token (xapp-…)", "SLACK_APP_TOKEN")?;
                let allowlist = gather_dm_allowlist(id, "Slack member id, e.g. U0123ABC")?;
                with_access(
                    json!({ "botToken": bot_token, "appToken": app_token }),
                    &allowlist,
                )
            }
            "discord" => {
                let token = gather_channel_secret("Discord bot token", "DISCORD_BOT_TOKEN")?;
                let allowlist =
                    gather_dm_allowlist(id, "Discord user id (enable Developer Mode to copy)")?;
                with_access(json!({ "botToken": token }), &allowlist)
            }
            "lark" => {
                let app_id = prompt("Lark app id (cli_…):")?;
                if app_id.is_empty() {
                    println!("Skipped: an app id is required.");
                    continue;
                }
                let app_secret = gather_channel_secret("Lark app secret", "LARK_APP_SECRET")?;
                let allowlist = gather_dm_allowlist(id, "Lark open_id, e.g. ou_…")?;
                with_access(
                    json!({ "appId": app_id, "appSecret": app_secret }),
                    &allowlist,
                )
            }
            "matrix" => {
                let homeserver = loop {
                    let url = prompt("Matrix homeserver URL (e.g. https://matrix.org):")?;
                    if url.starts_with("http://") || url.starts_with("https://") {
                        break url;
                    }
                    println!("The homeserver URL must start with http:// or https://.");
                };
                let token = gather_channel_secret("Matrix access token", "MATRIX_ACCESS_TOKEN")?;
                let user_id = prompt("Your Matrix user id (optional, e.g. @you:matrix.org):")?;
                let allowlist = gather_dm_allowlist(id, "Matrix user id, e.g. @you:matrix.org")?;
                let mut channel = json!({ "homeserver": homeserver, "accessToken": token });
                if !user_id.is_empty() {
                    channel["userId"] = json!(user_id);
                }
                with_access(channel, &allowlist)
            }
            _ => unreachable!("channel menu and handler list are in sync"),
        };
        configured.retain(|(existing, _)| existing != id);
        configured.push((id.to_string(), channel));
        println!("Channel '{id}' added.");
    }
    Ok(configured)
}

/// Merge a new provider into an existing config file instead of rewriting it.
///
/// The gateway section and every unrelated key survive untouched. Plain JSON
/// only — a `.json5` config must be edited by hand.
fn merge_provider_into_config(config_path: &Path, choices: &SetupChoices) -> Result<(), CliError> {
    let text = std::fs::read_to_string(config_path)?;
    let mut config: Value = serde_json::from_str(&text).map_err(|e| {
        CliError::Other(format!(
            "could not parse {} as JSON (json5 configs cannot be patched automatically): {e}",
            config_path.display()
        ))
    })?;
    let root = config
        .as_object_mut()
        .ok_or_else(|| CliError::Other("config root is not an object".to_string()))?;

    let models = root
        .entry("models")
        .or_insert_with(|| Value::Object(Map::new()));
    if !models.is_object() {
        *models = Value::Object(Map::new());
    }
    let models = models.as_object_mut().expect("models is an object");

    let providers = models
        .entry("providers")
        .or_insert_with(|| Value::Object(Map::new()));
    if !providers.is_object() {
        *providers = Value::Object(Map::new());
    }
    providers
        .as_object_mut()
        .expect("providers is an object")
        .insert(choices.provider_id.clone(), provider_config_json(choices));

    let aliases = models
        .entry("aliases")
        .or_insert_with(|| Value::Object(Map::new()));
    if !aliases.is_object() {
        *aliases = Value::Object(Map::new());
    }
    aliases
        .as_object_mut()
        .expect("aliases is an object")
        .insert(
            choices.alias.clone(),
            json!(format!("{}/{}", choices.provider_id, choices.default_model)),
        );

    let backup_path = config_path.with_extension("json.bak");
    std::fs::copy(config_path, &backup_path)?;
    println!("Existing config backed up to {}", backup_path.display());

    let merged = serde_json::to_string_pretty(&config)
        .map_err(|e| CliError::Other(format!("failed to serialize config: {e}")))?;
    // Validate before writing.
    let _ = legion_core::config::Config::from_json(&merged)?;
    std::fs::write(config_path, merged)?;
    Ok(())
}

/// Offer and run the advisory live connection test.
async fn maybe_test_connection(choices: &SetupChoices) -> Result<(), CliError> {
    if !prompt_yes_no("Test the connection now?", true)? {
        return Ok(());
    }
    print!("Testing... ");
    std::io::stdout().flush()?;
    match test_connection(choices).await {
        TestOutcome::Verified => {
            println!("ok.");
            Ok(())
        }
        TestOutcome::Unverifiable(note) => {
            println!("skipped: {note}");
            Ok(())
        }
        TestOutcome::Failed(reason) => {
            println!("failed: {reason}");
            if prompt_yes_no("Save the configuration anyway?", false)? {
                Ok(())
            } else {
                Err(CliError::Other("setup aborted".to_string()))
            }
        }
    }
}

/// Insert or replace one profile in `auth-profiles.json`, preserving the rest.
fn merge_auth_profile(
    auth_path: &Path,
    profile_name: &str,
    profile: &Value,
) -> Result<(), CliError> {
    let mut auth: Value = if auth_path.is_file() {
        match std::fs::read_to_string(auth_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
        {
            Some(Value::Object(existing)) => Value::Object(existing),
            _ => {
                // Unreadable or invalid: back it up and start fresh.
                let backup = auth_path.with_extension("json.bak");
                std::fs::copy(auth_path, &backup)?;
                println!("Existing auth profiles backed up to {}", backup.display());
                json!({})
            }
        }
    } else {
        json!({})
    };

    let root = auth.as_object_mut().expect("auth root is an object");
    let profiles = root
        .entry("profiles")
        .or_insert_with(|| Value::Object(Map::new()));
    if !profiles.is_object() {
        *profiles = Value::Object(Map::new());
    }
    profiles
        .as_object_mut()
        .expect("profiles is an object")
        .insert(profile_name.to_string(), profile.clone());

    let text = serde_json::to_string_pretty(&auth)
        .map_err(|e| CliError::Other(format!("failed to serialize auth profiles: {e}")))?;
    std::fs::write(auth_path, text)?;

    // Auth profiles hold plaintext keys; keep them owner-only on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(auth_path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// launchd label / systemd unit name for the gateway service.
pub const DAEMON_LABEL: &str = "com.legion.gateway";

/// Whether the current platform supports daemon installation.
pub fn daemon_supported() -> bool {
    cfg!(target_os = "macos") || cfg!(target_os = "linux") || cfg!(target_os = "windows")
}

/// Minimal XML escaping for plist string values.
#[cfg(target_os = "macos")]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Build the daemon unit file path and contents for the current platform.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn daemon_unit(home_dir: &Path) -> Result<(std::path::PathBuf, String), CliError> {
    let exe = std::env::current_exe()?;
    let log_path = home_dir.join(".legion/gateway.log");

    #[cfg(target_os = "macos")]
    {
        let path = home_dir.join(format!("Library/LaunchAgents/{DAEMON_LABEL}.plist"));
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             \t<key>Label</key>\n\t<string>{DAEMON_LABEL}</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\
             \t\t<string>{}</string>\n\t\t<string>gateway</string>\n\
             \t\t<string>start</string>\n\t\t<string>--foreground</string>\n\
             \t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n\
             \t<key>KeepAlive</key>\n\t<true/>\n\
             \t<key>StandardOutPath</key>\n\t<string>{}</string>\n\
             \t<key>StandardErrorPath</key>\n\t<string>{}</string>\n\
             \t<key>EnvironmentVariables</key>\n\t<dict>\n\
             \t\t<key>PATH</key>\n\
             \t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>\n\
             \t</dict>\n\
             </dict>\n</plist>\n",
            xml_escape(&exe.display().to_string()),
            xml_escape(&log_path.display().to_string()),
            xml_escape(&log_path.display().to_string()),
        );
        Ok((path, content))
    }

    #[cfg(target_os = "linux")]
    {
        let path = home_dir.join(".config/systemd/user/legion-gateway.service");
        let content = format!(
            "[Unit]\nDescription=Legion Gateway\n\n\
             [Service]\n\
             ExecStart={} gateway start --foreground\n\
             Restart=always\n\
             StandardOutput=append:{}\n\
             StandardError=append:{}\n\n\
             [Install]\nWantedBy=default.target\n",
            exe.display(),
            log_path.display(),
            log_path.display(),
        );
        Ok((path, content))
    }
}

/// A single command within a daemon load attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoadCommand {
    program: String,
    args: Vec<String>,
    /// Best-effort commands (e.g. a pre-load `unload`) never fail the attempt.
    required: bool,
}

impl LoadCommand {
    fn new(program: &str, args: &[&str], required: bool) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            required,
        }
    }

    fn required(program: &str, args: &[&str]) -> Self {
        Self::new(program, args, true)
    }
}

/// One daemon load attempt: every required command must succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoadAttempt {
    commands: Vec<LoadCommand>,
    /// Human-readable note reported when the attempt succeeds.
    note: String,
}

/// Build the ordered load attempts for the platform service manager plus a
/// manual hint shown when every attempt fails.
///
/// `unit_path` is the unit file written by [`install_daemon`] (launchd /
/// systemd); Windows schedules a logon task and needs no file.
fn daemon_load_plan(unit_path: Option<&Path>) -> Result<(Vec<LoadAttempt>, String), CliError> {
    #[cfg(target_os = "macos")]
    {
        let path =
            unit_path.ok_or_else(|| CliError::Other("missing launchd plist path".to_string()))?;
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let path_str = path.display().to_string();
        let attempts = vec![
            LoadAttempt {
                commands: vec![LoadCommand::required(
                    "launchctl",
                    &["bootstrap", &domain, &path_str],
                )],
                note: format!("loaded via launchctl ({domain})"),
            },
            // Legacy path for older macOS or an already-loaded service:
            // best-effort unload first, then `load -w`.
            LoadAttempt {
                commands: vec![
                    LoadCommand::new("launchctl", &["unload", &path_str], false),
                    LoadCommand::required("launchctl", &["load", "-w", &path_str]),
                ],
                note: "loaded via launchctl load -w".to_string(),
            },
        ];
        let hint = format!(
            "launchctl failed; load it manually: launchctl bootstrap {domain} {}",
            path.display()
        );
        Ok((attempts, hint))
    }

    #[cfg(target_os = "linux")]
    {
        let path =
            unit_path.ok_or_else(|| CliError::Other("missing systemd unit path".to_string()))?;
        let attempts = vec![LoadAttempt {
            commands: vec![
                LoadCommand::required("systemctl", &["--user", "daemon-reload"]),
                LoadCommand::required(
                    "systemctl",
                    &["--user", "enable", "--now", "legion-gateway"],
                ),
            ],
            note: "enabled via systemctl --user".to_string(),
        }];
        let hint = format!(
            "systemctl failed; enable it manually: systemctl --user daemon-reload && \
             systemctl --user enable --now legion-gateway (unit at {})",
            path.display()
        );
        Ok((attempts, hint))
    }

    #[cfg(target_os = "windows")]
    {
        let _ = unit_path;
        let exe = std::env::current_exe()?;
        // Quotes around the executable path must survive inside the single
        // /tr argument so paths containing spaces still parse.
        let task = format!("\"{}\" gateway start --foreground", exe.display());
        let attempts = vec![LoadAttempt {
            commands: vec![LoadCommand::required(
                "schtasks",
                &[
                    "/create",
                    "/tn",
                    "LegionGateway",
                    "/tr",
                    &task,
                    "/sc",
                    "onlogon",
                    "/f",
                ],
            )],
            note: "scheduled task 'LegionGateway' created (runs at logon)".to_string(),
        }];
        let hint = format!(
            "schtasks failed; create it manually: schtasks /create /tn LegionGateway \
             /tr \"{task}\" /sc onlogon"
        );
        Ok((attempts, hint))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = unit_path;
        Err(CliError::Other(
            "daemon installation is not supported on this platform".to_string(),
        ))
    }
}

/// Execute load attempts in order; the first fully successful attempt wins.
fn execute_load_plan(attempts: &[LoadAttempt], manual_hint: &str) -> Result<String, CliError> {
    for attempt in attempts {
        let mut ok = true;
        for command in &attempt.commands {
            match std::process::Command::new(&command.program)
                .args(&command.args)
                .status()
            {
                Ok(status) if status.success() => {}
                _ if command.required => {
                    ok = false;
                    break;
                }
                _ => {}
            }
        }
        if ok {
            return Ok(attempt.note.clone());
        }
    }
    Err(CliError::Other(manual_hint.to_string()))
}

/// Write the daemon unit file where the platform uses one; Windows schedules
/// a logon task instead and returns `None`.
fn install_daemon(home_dir: &Path) -> Result<Option<std::path::PathBuf>, CliError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let (path, content) = daemon_unit(home_dir)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        Ok(Some(path))
    }

    #[cfg(target_os = "windows")]
    {
        let _ = home_dir;
        Ok(None)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = home_dir;
        Err(CliError::Other(
            "daemon installation is not supported on this platform".to_string(),
        ))
    }
}

/// Offer (interactive) or honor the flag for daemon installation.
///
/// Failures never abort setup: the unit file may still be written, and the
/// gateway can always be started manually. Returns `true` when the service
/// was installed and loaded (i.e. the gateway is already running).
fn maybe_install_daemon(interactive: bool, install_flag: bool, home_dir: &Path) -> bool {
    if !daemon_supported() {
        if install_flag {
            println!("Daemon installation is not supported on this platform.");
        }
        return false;
    }
    let wants = if install_flag {
        true
    } else if interactive {
        match prompt_yes_no(
            "Install the gateway as a background service (auto-start on login)?",
            false,
        ) {
            Ok(wants) => wants,
            Err(_) => return false,
        }
    } else {
        false
    };
    if !wants {
        return false;
    }
    match install_daemon(home_dir).and_then(|path| {
        let (attempts, hint) = daemon_load_plan(path.as_deref())?;
        execute_load_plan(&attempts, &hint).map(|note| (path, note))
    }) {
        Ok((Some(path), note)) => {
            println!("Gateway service installed: {} ({note})", path.display());
            true
        }
        Ok((None, note)) => {
            println!("Gateway service installed ({note})");
            true
        }
        Err(err) => {
            println!("Daemon installation incomplete: {err}");
            println!("You can always start the gateway manually with `legion gateway start`.");
            false
        }
    }
}

/// Placeholder prefix used to detect unconfigured API keys.
pub const KEY_PLACEHOLDER_PREFIX: &str = "YOUR_";

/// Check whether a key is missing, blank, or a known placeholder.
fn is_placeholder_key(key: &str) -> bool {
    let normalized = key.trim();
    normalized.is_empty()
        || normalized
            .to_ascii_uppercase()
            .starts_with(KEY_PLACEHOLDER_PREFIX)
}

/// Check whether the user has completed the first-time setup.
///
/// Returns `true` if any of the following are true:
/// - `~/.legion/legion.json` does not exist or is not valid JSON
/// - the config declares no model provider
/// - any provider that needs credentials (everything except Ollama) has no
///   matching auth profile for agent `main`, or its key/secret is missing,
///   empty, or a known placeholder
pub fn is_setup_needed(home_dir: &Path) -> bool {
    let config_path = home_dir.join(".legion/legion.json");
    let config_text = match std::fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(_) => return true,
    };
    let config: Value = match serde_json::from_str(&config_text) {
        Ok(value) => value,
        Err(_) => return true,
    };

    let providers = match config
        .get("models")
        .and_then(|m| m.get("providers"))
        .and_then(|p| p.as_object())
    {
        Some(providers) if !providers.is_empty() => providers,
        _ => return true,
    };

    let auth_path = home_dir.join(".legion/agents/main/agent/auth-profiles.json");
    let auth: Value = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    let profiles = auth.get("profiles").and_then(|p| p.as_object());

    for provider in providers.values() {
        let kind = provider
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("openai");
        // Local Ollama needs no credentials.
        if kind == "ollama" {
            continue;
        }
        let profile_name = provider
            .get("authProfile")
            .and_then(|a| a.as_str())
            .unwrap_or("");
        let Some(profile) = profiles.and_then(|p| p.get(profile_name)) else {
            return true;
        };
        let profile_type = profile
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("api_key");
        match profile_type {
            "api_key" => {
                let key = profile.get("key").and_then(|k| k.as_str()).unwrap_or("");
                if is_placeholder_key(key) {
                    return true;
                }
            }
            "aws_sigv4" => {
                for field in ["access_key", "secret_key"] {
                    let value = profile.get(field).and_then(|v| v.as_str()).unwrap_or("");
                    if value.trim().is_empty() {
                        return true;
                    }
                }
            }
            // OAuth and future profile types count as configured.
            _ => {}
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn options_for(provider: &str, api_key: &str) -> SetupOptions {
        SetupOptions {
            provider: Some(provider.to_string()),
            api_key: Some(api_key.to_string()),
            gateway_token: Some("gw-test".to_string()),
            bind_host: Some("127.0.0.1".to_string()),
            port: Some(18789),
            ..SetupOptions::default()
        }
    }

    #[test]
    fn generate_token_produces_url_safe_string() {
        let token = generate_token();
        assert_eq!(token.len(), TOKEN_BYTES);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn presets_cover_all_router_kinds() {
        for kind in [
            "openai",
            "anthropic",
            "gemini",
            "ollama",
            "openrouter",
            "bedrock",
        ] {
            assert!(
                presets().iter().any(|p| p.kind == kind),
                "no preset covers kind {kind}"
            );
        }
        for preset in presets() {
            assert!(preset_by_key(preset.key).is_some());
        }
    }

    #[test]
    fn build_config_uses_options_or_defaults() {
        let opts = SetupOptions {
            provider: Some("minimax".to_string()),
            api_key: Some("sk-test".to_string()),
            gateway_token: Some("my-token".to_string()),
            bind_host: Some("0.0.0.0".to_string()),
            port: Some(8080),
            ..SetupOptions::default()
        };
        let choices = SetupChoices::from_options(&opts).unwrap();
        let config = build_config(&choices, &opts);
        assert_eq!(config.gateway.auth.token, Some("my-token".to_string()));
        assert_eq!(config.gateway.bind_host, "0.0.0.0");
        assert_eq!(config.gateway.port, 8080);
        assert_eq!(config.agents.defaults.model, Some("minimax".to_string()));
        assert!(config.models.providers.contains_key("minimax-openai"));
        assert_eq!(
            config.models.aliases.get("minimax").unwrap(),
            "minimax-openai/MiniMax-M3"
        );
    }

    #[test]
    fn build_config_generates_token_when_missing() {
        let opts = options_for("minimax", "sk-test");
        let mut opts = opts;
        opts.gateway_token = None;
        let choices = SetupChoices::from_options(&opts).unwrap();
        let config = build_config(&choices, &opts);
        let token = config.gateway.auth.token.unwrap();
        assert!(!token.is_empty());
        assert_eq!(token.len(), TOKEN_BYTES);
    }

    #[test]
    fn build_config_json_includes_channels_only_when_present() {
        let opts = options_for("minimax", "sk-test");
        let choices = SetupChoices::from_options(&opts).unwrap();

        let without: Value =
            serde_json::from_str(&build_config_json(&choices, &opts, &[])).unwrap();
        assert!(without.get("channels").is_none());

        let channels = vec![(
            "telegram".to_string(),
            with_access(json!({ "token": "123:ABC" }), &["42".to_string()]),
        )];
        let with: Value =
            serde_json::from_str(&build_config_json(&choices, &opts, &channels)).unwrap();
        let telegram = &with["channels"]["telegram"];
        assert_eq!(telegram["token"], "123:ABC");
        assert_eq!(telegram["access"]["dmPolicy"], "allowlist");
        assert_eq!(telegram["access"]["allowlist"][0], "42");
        // The generated config must still pass schema validation.
        legion_core::config::Config::from_json(&build_config_json(&choices, &opts, &channels))
            .unwrap();
    }

    #[test]
    fn with_access_is_a_no_op_for_empty_allowlist() {
        let channel = with_access(json!({ "token": "t" }), &[]);
        assert!(channel.get("access").is_none());
    }

    #[test]
    fn merge_provider_into_config_preserves_unrelated_keys() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("legion.json");
        let existing = json!({
            "gateway": {
                "bindHost": "127.0.0.1",
                "port": 18789,
                "auth": { "mode": "token", "token": "t" }
            },
            "agents": { "defaults": { "model": "minimax" } },
            "models": {
                "providers": {
                    "minimax-openai": {
                        "id": "minimax-openai",
                        "kind": "openai",
                        "authProfile": "minimax-default"
                    }
                },
                "aliases": { "minimax": "minimax-openai/MiniMax-M3" }
            },
            "subagents": { "maxConcurrent": 2 }
        });
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let choices = SetupChoices::from_options(&options_for("openai", "sk-two")).unwrap();
        merge_provider_into_config(&config_path, &choices).unwrap();

        let merged: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            merged["models"]["providers"]
                .get("minimax-openai")
                .is_some()
        );
        assert!(merged["models"]["providers"].get("openai").is_some());
        assert_eq!(merged["models"]["aliases"]["openai"], "openai/gpt-4o");
        // Unrelated keys survive untouched.
        assert_eq!(merged["agents"]["defaults"]["model"], "minimax");
        assert_eq!(merged["subagents"]["maxConcurrent"], 2);
        assert!(temp.path().join("legion.json.bak").is_file());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn daemon_unit_references_gateway_foreground_and_log() {
        let temp = TempDir::new().unwrap();
        let (path, content) = daemon_unit(temp.path()).unwrap();
        let exe = std::env::current_exe().unwrap();
        let log = temp.path().join(".legion/gateway.log");
        assert!(content.contains("gateway"));
        assert!(content.contains("--foreground"));
        assert!(content.contains(&exe.display().to_string()));
        assert!(content.contains(&log.display().to_string()));
        #[cfg(target_os = "macos")]
        {
            assert!(content.contains(&format!("<string>{DAEMON_LABEL}</string>")));
            assert!(path.ends_with(format!("Library/LaunchAgents/{DAEMON_LABEL}.plist")));
        }
        #[cfg(target_os = "linux")]
        {
            assert!(content.contains("WantedBy=default.target"));
            assert!(path.ends_with(".config/systemd/user/legion-gateway.service"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn daemon_load_plan_macos_bootstrap_then_legacy_load() {
        let path = std::path::PathBuf::from("/tmp/com.legion.gateway.plist");
        let (attempts, hint) = daemon_load_plan(Some(&path)).unwrap();
        assert_eq!(attempts.len(), 2);

        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let primary = &attempts[0];
        assert_eq!(primary.commands.len(), 1);
        assert_eq!(primary.commands[0].program, "launchctl");
        assert_eq!(
            primary.commands[0].args,
            vec!["bootstrap", domain.as_str(), path.to_str().unwrap()]
        );
        assert!(primary.commands[0].required);

        let legacy = &attempts[1];
        assert_eq!(legacy.commands.len(), 2);
        assert!(!legacy.commands[0].required, "unload is best-effort");
        assert_eq!(
            legacy.commands[0].args,
            vec!["unload", path.to_str().unwrap()]
        );
        assert!(legacy.commands[1].required);
        assert_eq!(
            legacy.commands[1].args,
            vec!["load", "-w", path.to_str().unwrap()]
        );
        assert!(hint.contains("launchctl bootstrap"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn daemon_load_plan_linux_systemctl_user_enable() {
        let path = std::path::PathBuf::from("/tmp/legion-gateway.service");
        let (attempts, hint) = daemon_load_plan(Some(&path)).unwrap();
        assert_eq!(attempts.len(), 1);
        let commands = &attempts[0].commands;
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].program, "systemctl");
        assert_eq!(commands[0].args, vec!["--user", "daemon-reload"]);
        assert_eq!(
            commands[1].args,
            vec!["--user", "enable", "--now", "legion-gateway"]
        );
        assert!(commands.iter().all(|c| c.required));
        assert!(hint.contains("systemctl --user"));
    }

    #[cfg(unix)]
    #[test]
    fn execute_load_plan_first_successful_attempt_wins() {
        let attempts = vec![
            LoadAttempt {
                commands: vec![LoadCommand::required("/usr/bin/false", &[])],
                note: "bad".to_string(),
            },
            LoadAttempt {
                commands: vec![LoadCommand::required("/usr/bin/true", &[])],
                note: "good".to_string(),
            },
        ];
        assert_eq!(execute_load_plan(&attempts, "hint").unwrap(), "good");
    }

    #[cfg(unix)]
    #[test]
    fn execute_load_plan_ignores_best_effort_failures() {
        let attempts = vec![LoadAttempt {
            commands: vec![
                LoadCommand::new("/nonexistent-legion-test-binary", &[], false),
                LoadCommand::required("/usr/bin/true", &[]),
            ],
            note: "ok".to_string(),
        }];
        assert_eq!(execute_load_plan(&attempts, "hint").unwrap(), "ok");
    }

    #[cfg(unix)]
    #[test]
    fn execute_load_plan_reports_manual_hint_when_all_fail() {
        let attempts = vec![LoadAttempt {
            commands: vec![LoadCommand::required("/usr/bin/false", &[])],
            note: "bad".to_string(),
        }];
        let err = execute_load_plan(&attempts, "do it by hand").unwrap_err();
        assert!(err.to_string().contains("do it by hand"));
    }

    #[test]
    fn from_options_supports_each_api_key_preset() {
        for (key, provider_id) in [
            ("minimax", "minimax-openai"),
            ("openai", "openai"),
            ("anthropic", "anthropic"),
            ("gemini", "gemini"),
            ("openrouter", "openrouter"),
        ] {
            let opts = options_for(key, "sk-test");
            let choices = SetupChoices::from_options(&opts).unwrap();
            assert_eq!(choices.provider_id, provider_id);
            assert_eq!(choices.auth_profile_name, format!("{key}-default"));
            assert_eq!(
                choices.auth_profile.unwrap(),
                json!({ "type": "api_key", "key": "sk-test" })
            );
        }
    }

    #[test]
    fn from_options_ollama_needs_no_key() {
        let opts = SetupOptions {
            provider: Some("ollama".to_string()),
            ..SetupOptions::default()
        };
        let choices = SetupChoices::from_options(&opts).unwrap();
        assert_eq!(choices.kind, "ollama");
        assert!(choices.auth_profile.is_none());
        let config = build_config(&choices, &opts);
        assert!(config.models.providers.contains_key("ollama"));
    }

    #[test]
    fn from_options_rejects_missing_key() {
        let opts = SetupOptions {
            provider: Some("openai".to_string()),
            ..SetupOptions::default()
        };
        let err = SetupChoices::from_options(&opts).unwrap_err();
        assert!(err.to_string().contains("--api-key is required"));
    }

    #[test]
    fn from_options_rejects_placeholder_key() {
        let opts = options_for("openai", "YOUR_OPENAI_API_KEY");
        let err = SetupChoices::from_options(&opts).unwrap_err();
        assert!(err.to_string().contains("placeholder"));
    }

    #[test]
    fn from_options_rejects_unknown_provider() {
        let opts = SetupOptions {
            provider: Some("wat".to_string()),
            api_key: Some("sk".to_string()),
            ..SetupOptions::default()
        };
        let err = SetupChoices::from_options(&opts).unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn from_options_requires_provider_without_key() {
        let opts = SetupOptions::default();
        let err = SetupChoices::from_options(&opts).unwrap_err();
        assert!(err.to_string().contains("--provider is required"));
    }

    #[test]
    fn from_options_defaults_to_minimax_for_bare_api_key() {
        let opts = SetupOptions {
            api_key: Some("sk-test".to_string()),
            ..SetupOptions::default()
        };
        let choices = SetupChoices::from_options(&opts).unwrap();
        assert_eq!(choices.provider_id, "minimax-openai");
    }

    #[test]
    fn from_options_rejects_bedrock_non_interactive() {
        let opts = SetupOptions {
            provider: Some("bedrock".to_string()),
            ..SetupOptions::default()
        };
        let err = SetupChoices::from_options(&opts).unwrap_err();
        assert!(err.to_string().contains("interactively"));
    }

    #[test]
    fn from_options_custom_requires_url_model_and_key() {
        let opts = SetupOptions {
            provider: Some("custom".to_string()),
            api_key: Some("sk".to_string()),
            ..SetupOptions::default()
        };
        assert!(SetupChoices::from_options(&opts).is_err());

        let opts = SetupOptions {
            provider: Some("custom".to_string()),
            api_key: Some("sk".to_string()),
            base_url: Some("https://api.example.com/v1".to_string()),
            model: Some("my-model".to_string()),
            ..SetupOptions::default()
        };
        let choices = SetupChoices::from_options(&opts).unwrap();
        assert_eq!(choices.provider_id, "custom");
        assert_eq!(choices.kind, "openai");
        assert_eq!(
            choices.base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        let config = build_config(&choices, &opts);
        assert_eq!(
            config.models.aliases.get("custom").unwrap(),
            "custom/my-model"
        );
    }

    #[tokio::test]
    async fn non_interactive_setup_writes_config_and_auth_files() {
        let temp = TempDir::new().unwrap();
        let opts = options_for("openai", "sk-test");

        run_setup(false, opts, temp.path()).await.unwrap();

        let config_path = temp.path().join(".legion/legion.json");
        let auth_path = temp
            .path()
            .join(".legion/agents/main/agent/auth-profiles.json");

        assert!(config_path.exists());
        assert!(auth_path.exists());

        let config_text = std::fs::read_to_string(&config_path).unwrap();
        let config: legion_core::config::Config = serde_json::from_str(&config_text).unwrap();
        assert_eq!(config.gateway.auth.token, Some("gw-test".to_string()));
        assert!(config.models.providers.contains_key("openai"));

        let auth_text = std::fs::read_to_string(&auth_path).unwrap();
        let auth: Value = serde_json::from_str(&auth_text).unwrap();
        assert_eq!(auth["profiles"]["openai-default"]["key"], "sk-test");

        // Workspace is created and seeded.
        assert!(temp.path().join(".legion/workspace/AGENTS.md").is_file());

        // Auth file is owner-only on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&auth_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn non_interactive_setup_refuses_to_overwrite_without_force() {
        let temp = TempDir::new().unwrap();
        let opts = options_for("minimax", "sk-one");
        run_setup(false, opts, temp.path()).await.unwrap();

        let opts = options_for("openai", "sk-two");
        let err = run_setup(false, opts, temp.path()).await.unwrap_err();
        assert!(err.to_string().contains("--force"));
    }

    #[tokio::test]
    async fn forced_rerun_backs_up_config_and_merges_auth_profiles() {
        let temp = TempDir::new().unwrap();
        let opts = options_for("minimax", "sk-one");
        run_setup(false, opts, temp.path()).await.unwrap();

        let mut opts = options_for("openai", "sk-two");
        opts.force = true;
        run_setup(false, opts, temp.path()).await.unwrap();

        // Old config was backed up.
        let backup = temp.path().join(".legion/legion.json.bak");
        assert!(backup.is_file());
        let backup_text = std::fs::read_to_string(backup).unwrap();
        assert!(backup_text.contains("minimax-openai"));

        // New config points at openai.
        let config_text = std::fs::read_to_string(temp.path().join(".legion/legion.json")).unwrap();
        assert!(config_text.contains("\"openai\""));
        assert!(!config_text.contains("minimax-openai"));

        // Both auth profiles survive.
        let auth_text = std::fs::read_to_string(
            temp.path()
                .join(".legion/agents/main/agent/auth-profiles.json"),
        )
        .unwrap();
        let auth: Value = serde_json::from_str(&auth_text).unwrap();
        let profiles = auth["profiles"].as_object().unwrap();
        assert_eq!(profiles["minimax-default"]["key"], "sk-one");
        assert_eq!(profiles["openai-default"]["key"], "sk-two");
    }

    #[tokio::test]
    async fn ollama_setup_writes_no_auth_file() {
        let temp = TempDir::new().unwrap();
        let opts = SetupOptions {
            provider: Some("ollama".to_string()),
            gateway_token: Some("gw".to_string()),
            ..SetupOptions::default()
        };
        run_setup(false, opts, temp.path()).await.unwrap();
        assert!(
            !temp
                .path()
                .join(".legion/agents/main/agent/auth-profiles.json")
                .exists()
        );
    }

    fn write_config(temp: &TempDir, provider_id: &str, kind: &str, profile: &str) {
        let config_path = temp.path().join(".legion/legion.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let config = json!({
            "gateway": { "auth": { "mode": "token", "token": "t" } },
            "models": {
                "providers": {
                    provider_id: {
                        "id": provider_id,
                        "kind": kind,
                        "authProfile": profile,
                        "defaultModel": "m"
                    }
                }
            }
        });
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    }

    fn write_auth(temp: &TempDir, profile: &str, value: Value) {
        let auth_path = temp
            .path()
            .join(".legion/agents/main/agent/auth-profiles.json");
        std::fs::create_dir_all(auth_path.parent().unwrap()).unwrap();
        let auth = json!({ "profiles": { profile: value } });
        std::fs::write(&auth_path, serde_json::to_string_pretty(&auth).unwrap()).unwrap();
    }

    #[test]
    fn setup_needed_when_nothing_exists() {
        let temp = TempDir::new().unwrap();
        assert!(is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_needed_when_config_has_no_providers() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join(".legion/legion.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, r#"{"gateway": {"auth": {"mode": "token"}}}"#).unwrap();
        assert!(is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_needed_when_auth_profile_missing() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "openai", "openai", "openai-default");
        assert!(is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_needed_when_key_is_placeholder() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "openai", "openai", "openai-default");
        write_auth(
            &temp,
            "openai-default",
            json!({ "type": "api_key", "key": "YOUR_OPENAI_API_KEY_HERE" }),
        );
        assert!(is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_needed_when_key_is_lowercase_placeholder() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "minimax-openai", "openai", "minimax-default");
        write_auth(
            &temp,
            "minimax-default",
            json!({ "type": "api_key", "key": "your_minimax_api_key" }),
        );
        assert!(is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_needed_when_key_is_empty_or_whitespace() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "openai", "openai", "openai-default");
        write_auth(
            &temp,
            "openai-default",
            json!({ "type": "api_key", "key": "   " }),
        );
        assert!(is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_not_needed_when_key_is_configured() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "openai", "openai", "openai-default");
        write_auth(
            &temp,
            "openai-default",
            json!({ "type": "api_key", "key": "sk-real-key" }),
        );
        assert!(!is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_not_needed_for_ollama_without_auth() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "ollama", "ollama", "ollama");
        assert!(!is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_not_needed_with_env_ref_key() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "anthropic", "anthropic", "anthropic-default");
        write_auth(
            &temp,
            "anthropic-default",
            json!({ "type": "api_key", "key": "${ANTHROPIC_API_KEY}" }),
        );
        assert!(!is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_needed_when_aws_credentials_incomplete() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "bedrock", "bedrock", "bedrock-default");
        write_auth(
            &temp,
            "bedrock-default",
            json!({ "type": "aws_sigv4", "access_key": "AK", "secret_key": "", "region": "us-east-1" }),
        );
        assert!(is_setup_needed(temp.path()));
    }

    #[test]
    fn setup_not_needed_with_full_aws_credentials() {
        let temp = TempDir::new().unwrap();
        write_config(&temp, "bedrock", "bedrock", "bedrock-default");
        write_auth(
            &temp,
            "bedrock-default",
            json!({ "type": "aws_sigv4", "access_key": "AK", "secret_key": "sk", "region": "us-east-1" }),
        );
        assert!(!is_setup_needed(temp.path()));
    }
}
