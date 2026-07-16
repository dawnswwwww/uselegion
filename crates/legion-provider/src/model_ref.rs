use crate::types::{ProviderError, ResolvedModelRef};
use std::collections::HashMap;

/// Parses a model reference of the form `provider/model`.
///
/// OpenRouter-style references may contain additional slashes in the model
/// portion (e.g. `openrouter/moonshotai/kimi-k2`). The first segment is always
/// treated as the provider id and the remainder as the model name.
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

    let model_name = segments.collect::<Vec<_>>().join("/");
    if model_name.is_empty() {
        return Err(ProviderError::InvalidModelRef(format!(
            "model reference '{model_ref}' is missing a model name"
        )));
    }

    Ok(ResolvedModelRef {
        provider_id,
        model_name,
    })
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
}
