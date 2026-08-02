use crate::types::{ProviderError, ResolvedModelRef};
use legion_core::config::Config;
use std::collections::HashMap;

/// Built-in fallback used when neither the agent nor `agents.defaults`
/// configures a model.
pub const DEFAULT_MODEL: &str = "openai/gpt-4o";

/// Resolve the model reference for an agent.
///
/// `main` (and any agent without its own model) uses `agents.defaults.model`;
/// otherwise the agent's entry in `agents.list` wins. Falls back to
/// [`DEFAULT_MODEL`] when nothing is configured.
pub fn resolve_agent_model(config: &Config, agent_id: &str) -> String {
    if agent_id == "main" {
        config.agents.defaults.model.clone()
    } else {
        config
            .agents
            .list
            .iter()
            .find(|a| a.id == agent_id)
            .and_then(|a| a.model.clone())
            .or_else(|| config.agents.defaults.model.clone())
    }
    .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Parses a model reference of the form `provider/model`.
///
/// OpenRouter-style references may contain additional slashes in the model
/// portion (e.g. `openrouter/moonshotai/kimi-k2`). The first segment is always
/// treated as the provider id and the remainder as the model name.
///
/// A trailing context-window override suffix of the form `[<n>]`, `[<n>k]`, or
/// `[<n>m]` (e.g. `minimax/MiniMax-M3[1m]`) is stripped from the model name so
/// the API receives a clean identifier; the override is re-parsed on the
/// compaction side via [`parse_context_window_suffix`].
pub fn parse_model_ref(model_ref: &str) -> Result<ResolvedModelRef, ProviderError> {
    if model_ref.is_empty() {
        return Err(ProviderError::InvalidModelRef(
            "model reference is empty".to_string(),
        ));
    }

    let mut segments = model_ref.split('/');
    let provider_id = segments
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::InvalidModelRef(model_ref.to_string()))?
        .to_string();

    let raw_model_name = segments.collect::<Vec<_>>().join("/");
    if raw_model_name.is_empty() {
        return Err(ProviderError::InvalidModelRef(format!(
            "model reference '{model_ref}' is missing a model name"
        )));
    }

    // Strip an optional context-window suffix (`[1m]`, `[512k]`, `[200000]`).
    let model_name = parse_context_window_suffix(&raw_model_name).0;

    Ok(ResolvedModelRef {
        provider_id,
        model_name,
    })
}

/// Split a model name that may carry a trailing `[...]` context-window override
/// into the clean name and the parsed override.
///
/// The bracket group must be the final segment of the name (e.g.
/// `MiniMax-M3[1m]`). Accepted inner forms: a plain integer (`128000`), an
/// integer with a `k`/`K` suffix (thousands), or an `m`/`M` suffix (millions).
/// A malformed suffix (e.g. `[abc]`, `[1.5m]`) is ignored: the name is returned
/// unchanged and the override is `None`, so a typo degrades to the default
/// window rather than erroring.
pub fn parse_context_window_suffix(model_name: &str) -> (String, Option<usize>) {
    let Some((prefix, bracket)) = model_name.rsplit_once('[') else {
        return (model_name.to_string(), None);
    };
    // The bracket group must end the name and have a non-empty prefix
    // (a bare `[1m]` is not a valid model name).
    let Some(inner) = bracket.strip_suffix(']') else {
        return (model_name.to_string(), None);
    };
    if prefix.is_empty() {
        return (model_name.to_string(), None);
    }

    match parse_window_value(inner) {
        Some(n) => (prefix.to_string(), Some(n)),
        None => (model_name.to_string(), None),
    }
}

/// Parse an inner suffix body (`"1m"`, `"512k"`, `"200000"`) into a token count.
fn parse_window_value(inner: &str) -> Option<usize> {
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }

    let (digits, multiplier) = match inner.as_bytes().last() {
        Some(b'k' | b'K') => (&inner[..inner.len() - 1], 1_000),
        Some(b'm' | b'M') => (&inner[..inner.len() - 1], 1_000_000),
        _ => (inner, 1),
    };

    let n: usize = digits.parse().ok()?;
    n.checked_mul(multiplier)
}

/// Resolve a model reference, expanding aliases recursively.
///
/// If the reference contains a `/`, it is returned as-is. Otherwise it is
/// looked up in `aliases`. The resolved alias is parsed again, so alias chains
/// are supported as long as they terminate in a `provider/model` form.
pub fn resolve_model_ref(
    model_ref: &str,
    aliases: &HashMap<String, String>,
) -> Result<ResolvedModelRef, ProviderError> {
    let mut current = model_ref.to_string();
    let mut depth = 0;
    const MAX_DEPTH: usize = 8;

    loop {
        if current.contains('/') {
            return parse_model_ref(&current);
        }

        match aliases.get(&current) {
            Some(target) => {
                current = target.clone();
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(ProviderError::InvalidModelRef(format!(
                        "alias resolution exceeded max depth for '{model_ref}'"
                    )));
                }
            }
            None => {
                return Err(ProviderError::InvalidModelRef(format!(
                    "unknown model alias or missing provider: '{model_ref}'"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agent_model_prefers_agent_then_default() {
        let config = Config::from_json(
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "agents": {
                    "defaults": { "model": "openai/default-model" },
                    "list": [
                        { "id": "main", "model": "anthropic/main-model" },
                        { "id": "researcher", "model": "anthropic/agent-model" },
                        { "id": "writer" }
                    ]
                }
            }"#,
        )
        .expect("test config parses");

        // An agent's own model wins over the default.
        assert_eq!(
            resolve_agent_model(&config, "researcher"),
            "anthropic/agent-model"
        );
        // A listed agent without a model, and unlisted agents, use the default.
        assert_eq!(
            resolve_agent_model(&config, "writer"),
            "openai/default-model"
        );
        assert_eq!(
            resolve_agent_model(&config, "ghost"),
            "openai/default-model"
        );
        // "main" always uses the default, even when listed with its own model.
        assert_eq!(resolve_agent_model(&config, "main"), "openai/default-model");

        // No model configured anywhere falls back to the built-in default.
        let bare = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#)
            .expect("test config parses");
        assert_eq!(resolve_agent_model(&bare, "researcher"), "openai/gpt-4o");
    }

    #[test]
    fn parse_simple_provider_model() {
        let parsed = parse_model_ref("anthropic/claude-sonnet-4-6").unwrap();
        assert_eq!(parsed.provider_id, "anthropic");
        assert_eq!(parsed.model_name, "claude-sonnet-4-6");
    }

    #[test]
    fn parse_openrouter_style_with_extra_slashes() {
        let parsed = parse_model_ref("openrouter/moonshotai/kimi-k2").unwrap();
        assert_eq!(parsed.provider_id, "openrouter");
        assert_eq!(parsed.model_name, "moonshotai/kimi-k2");
    }

    #[test]
    fn parse_strips_context_window_suffix() {
        let parsed = parse_model_ref("minimax/MiniMax-M3[1m]").unwrap();
        assert_eq!(parsed.provider_id, "minimax");
        assert_eq!(parsed.model_name, "MiniMax-M3");
    }

    #[test]
    fn parse_strips_suffix_from_openrouter_model() {
        let parsed = parse_model_ref("openrouter/moonshotai/kimi-k2[512k]").unwrap();
        assert_eq!(parsed.provider_id, "openrouter");
        assert_eq!(parsed.model_name, "moonshotai/kimi-k2");
    }

    #[test]
    fn parse_rejects_empty_ref() {
        assert!(parse_model_ref("").is_err());
    }

    #[test]
    fn parse_rejects_missing_model_name() {
        assert!(parse_model_ref("openai/").is_err());
    }

    #[test]
    fn parse_rejects_missing_provider() {
        assert!(parse_model_ref("/gpt-4").is_err());
    }

    #[test]
    fn resolve_alias_to_provider_model() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        );

        let resolved = resolve_model_ref("claude", &aliases).unwrap();
        assert_eq!(resolved.provider_id, "anthropic");
        assert_eq!(resolved.model_name, "claude-sonnet-4-6");
    }

    #[test]
    fn resolve_alias_chain() {
        let mut aliases = HashMap::new();
        aliases.insert("fast".to_string(), "local".to_string());
        aliases.insert("local".to_string(), "local-ollama/qwen3:8b".to_string());

        let resolved = resolve_model_ref("fast", &aliases).unwrap();
        assert_eq!(resolved.provider_id, "local-ollama");
        assert_eq!(resolved.model_name, "qwen3:8b");
    }

    #[test]
    fn resolve_unknown_alias_fails() {
        let aliases = HashMap::new();
        assert!(resolve_model_ref("unknown", &aliases).is_err());
    }

    #[test]
    fn resolve_direct_provider_model_ignores_aliases() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "claude".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        );

        let resolved = resolve_model_ref("openai/gpt-4", &aliases).unwrap();
        assert_eq!(resolved.provider_id, "openai");
        assert_eq!(resolved.model_name, "gpt-4");
    }

    #[test]
    fn resolve_detects_alias_cycle() {
        let mut aliases = HashMap::new();
        aliases.insert("a".to_string(), "b".to_string());
        aliases.insert("b".to_string(), "a".to_string());

        assert!(resolve_model_ref("a", &aliases).is_err());
    }

    #[test]
    fn suffix_plain_number() {
        let (name, win) = parse_context_window_suffix("claude-sonnet-4[200000]");
        assert_eq!(name, "claude-sonnet-4");
        assert_eq!(win, Some(200_000));
    }

    #[test]
    fn suffix_k_suffix() {
        let (name, win) = parse_context_window_suffix("MiniMax-M3[512k]");
        assert_eq!(name, "MiniMax-M3");
        assert_eq!(win, Some(512_000));
    }

    #[test]
    fn suffix_m_suffix() {
        let (name, win) = parse_context_window_suffix("MiniMax-M3[1m]");
        assert_eq!(name, "MiniMax-M3");
        assert_eq!(win, Some(1_000_000));
    }

    #[test]
    fn suffix_case_insensitive() {
        assert_eq!(parse_context_window_suffix("m[2K]").1, Some(2_000));
        assert_eq!(parse_context_window_suffix("m[2M]").1, Some(2_000_000));
    }

    #[test]
    fn suffix_ignored_when_malformed() {
        // Non-numeric inner body keeps the name intact.
        let (name, win) = parse_context_window_suffix("m[abc]");
        assert_eq!(name, "m[abc]");
        assert_eq!(win, None);
        // Fractional values are not accepted (usize parse fails).
        let (name, win) = parse_context_window_suffix("m[1.5m]");
        assert_eq!(name, "m[1.5m]");
        assert_eq!(win, None);
    }

    #[test]
    fn suffix_ignored_when_not_trailing() {
        // A bracket group that does not end the name is left untouched.
        let (name, win) = parse_context_window_suffix("m[1m]extra");
        assert_eq!(name, "m[1m]extra");
        assert_eq!(win, None);
    }

    #[test]
    fn suffix_no_bracket() {
        let (name, win) = parse_context_window_suffix("plain-model");
        assert_eq!(name, "plain-model");
        assert_eq!(win, None);
    }
}
