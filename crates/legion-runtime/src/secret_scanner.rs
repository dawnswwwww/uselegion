use regex::Regex;
use std::sync::OnceLock;

/// Detects common secrets (API keys, tokens, passwords) in text.
///
/// Used by the auto-extractor to drop candidate facts that would otherwise
/// persist credentials into the memory store. Matching is intentionally
/// conservative: a hit means "treat as secret" and drop the whole fact.
#[derive(Debug, Clone, Copy, Default)]
pub struct SecretScanner;

impl SecretScanner {
    pub fn new() -> Self {
        Self
    }

    /// Return `true` if `text` contains a known secret pattern.
    pub fn contains_secret(&self, text: &str) -> bool {
        rules().iter().any(|re| re.is_match(text))
    }
}

fn rules() -> &'static [Regex] {
    static RULES: OnceLock<Vec<Regex>> = OnceLock::new();
    RULES.get_or_init(|| {
        const PATTERNS: &[&str] = &[
            // OpenAI / Anthropic style keys (`sk-...`, including `sk-ant-...`).
            r"sk-[A-Za-z0-9_-]{20,}",
            // GitHub classic / fine-grained tokens.
            r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            // AWS access key id.
            r"AKIA[0-9A-Z]{16}",
            // Bearer tokens (JWT-ish).
            r"Bearer [A-Za-z0-9._~+/=-]{20,}",
            // Assignment-style secrets with a minimum length to limit false
            // positives: `api_key = "..."`, `password: ...`, etc.
            r#"(?i)(?:api[_-]?key|api[_-]?secret|access[_-]?token|secret|password|passwd|pwd)\s*[:=]\s*[\"']?[A-Za-z0-9._~+/=-]{16,}"#,
        ];
        PATTERNS
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    tracing::warn!(pattern = p, error = %e, "invalid secret pattern skipped");
                    None
                }
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_openai_key() {
        let s = SecretScanner::new();
        assert!(s.contains_secret("OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123456"));
    }

    #[test]
    fn detects_anthropic_key() {
        let s = SecretScanner::new();
        assert!(s.contains_secret("sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234567890"));
    }

    #[test]
    fn detects_github_token() {
        let s = SecretScanner::new();
        assert!(s.contains_secret("ghp_abcdefghijklmnopqrstuvwxyz1234567890"));
        assert!(s.contains_secret("github_pat_abcdefghijklmnopqrstuvwxyz1234567890"));
    }

    #[test]
    fn detects_aws_key() {
        let s = SecretScanner::new();
        assert!(s.contains_secret("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn detects_assignment_secret() {
        let s = SecretScanner::new();
        assert!(s.contains_secret(r#"password = "hunter2hunter2hunter2""#));
        assert!(s.contains_secret("api_key: abcdef0123456789abcdef"));
    }

    #[test]
    fn ignores_clean_text() {
        let s = SecretScanner::new();
        assert!(!s.contains_secret("User prefers dark mode and uses Rust daily."));
        assert!(!s.contains_secret("Project deploys to staging on Fridays."));
    }

    #[test]
    fn ignores_short_assignment_values() {
        let s = SecretScanner::new();
        // Short values are below the length threshold and must not match.
        assert!(!s.contains_secret("password = ok"));
    }
}
