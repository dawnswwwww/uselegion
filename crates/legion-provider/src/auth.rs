use crate::types::ProviderError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// An authentication profile for a provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthProfile {
    ApiKey {
        /// The resolved API key. Environment variable references such as
        /// `${ANTHROPIC_API_KEY}` are expanded at load time.
        key: String,
    },
    #[serde(rename = "oauth")]
    OAuth {
        client_id: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        expires_at: Option<u64>,
    },
    /// AWS credentials for SigV4-signed providers (e.g. Bedrock).
    AwsSigv4 {
        access_key: String,
        secret_key: String,
        #[serde(default)]
        session_token: Option<String>,
        region: String,
    },
}

impl AuthProfile {
    /// Convenience constructor for tests.
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey { key: key.into() }
    }

    /// Convenience constructor for AWS SigV4 credentials.
    pub fn aws_sigv4(
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        session_token: Option<String>,
        region: impl Into<String>,
    ) -> Self {
        Self::AwsSigv4 {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            session_token,
            region: region.into(),
        }
    }

    /// Returns the API key if this profile is `ApiKey`.
    pub fn api_key_value(&self) -> Option<&str> {
        match self {
            AuthProfile::ApiKey { key } => Some(key),
            _ => None,
        }
    }

    /// Returns owned AWS credentials if this profile is `AwsSigv4`.
    pub fn aws_sigv4_value(&self) -> Option<crate::sigv4::AwsCreds> {
        match self {
            AuthProfile::AwsSigv4 {
                access_key,
                secret_key,
                session_token,
                region,
            } => Some(crate::sigv4::AwsCreds {
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                session_token: session_token.clone(),
                region: region.clone(),
            }),
            _ => None,
        }
    }
}

/// Container used by `auth-profiles.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct AuthProfilesFile {
    profiles: HashMap<String, serde_json::Value>,
}

/// Load auth profiles from `~/.legion/agents/<agentId>/agent/auth-profiles.json`.
pub fn load_auth_profiles(agent_id: &str) -> Result<HashMap<String, AuthProfile>, ProviderError> {
    let path = auth_profiles_path(agent_id);
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let contents = std::fs::read_to_string(&path).map_err(|e| {
        ProviderError::InvalidAuth(format!("failed to read {}: {e}", path.display()))
    })?;

    let raw: AuthProfilesFile = serde_json::from_str(&contents).map_err(|e| {
        ProviderError::InvalidAuth(format!("failed to parse {}: {e}", path.display()))
    })?;

    let mut profiles = HashMap::with_capacity(raw.profiles.len());
    for (name, value) in raw.profiles {
        let resolved = resolve_env_vars_in_value(&value)?;
        let profile: AuthProfile = serde_json::from_value(resolved).map_err(|e| {
            ProviderError::InvalidAuth(format!(
                "invalid auth profile '{name}' in {}: {e}",
                path.display()
            ))
        })?;
        profiles.insert(name, profile);
    }

    Ok(profiles)
}

/// Path to the auth-profiles.json file for an agent.
pub fn auth_profiles_path(agent_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!(
            ".legion/agents/{agent_id}/agent/auth-profiles.json"
        ))
}

/// Resolve environment variable references of the form `${VAR}` or
/// `${VAR:default}` inside a JSON value.
fn resolve_env_vars_in_value(
    value: &serde_json::Value,
) -> Result<serde_json::Value, ProviderError> {
    match value {
        serde_json::Value::String(s) => {
            if s.starts_with("${") && s.ends_with('}') && s.matches("${").count() == 1 {
                let inner = &s[2..s.len() - 1];
                let (var_name, default_value) = inner
                    .split_once(':')
                    .map_or((inner, None), |(n, v)| (n, Some(v)));

                match std::env::var(var_name) {
                    Ok(v) => Ok(serde_json::Value::String(v)),
                    Err(_) => match default_value {
                        Some(d) => Ok(serde_json::Value::String(d.to_string())),
                        None => Err(ProviderError::InvalidAuth(format!(
                            "environment variable '{var_name}' not found"
                        ))),
                    },
                }
            } else {
                Ok(serde_json::Value::String(s.clone()))
            }
        }
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(resolve_env_vars_in_value(item)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        serde_json::Value::Object(obj) => {
            let mut out = serde_json::Map::new();
            for (k, v) in obj {
                out.insert(k.clone(), resolve_env_vars_in_value(v)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn parse_api_key_profile() {
        let json = r#"{ "type": "api_key", "key": "sk-test" }"#;
        let profile: AuthProfile = serde_json::from_str(json).unwrap();
        assert_eq!(profile, AuthProfile::api_key("sk-test"));
    }

    #[test]
    fn parse_oauth_profile() {
        let json = r#"{
            "type": "oauth",
            "client_id": "client-123",
            "refresh_token": "rt",
            "access_token": "at",
            "expires_at": 123456
        }"#;
        let profile: AuthProfile = serde_json::from_str(json).unwrap();
        assert!(matches!(profile, AuthProfile::OAuth { .. }));
    }

    #[test]
    fn parse_aws_sigv4_profile() {
        let json = r#"{
            "type": "aws_sigv4",
            "access_key": "AKIDEXAMPLE",
            "secret_key": "secret",
            "region": "us-east-1"
        }"#;
        let profile: AuthProfile = serde_json::from_str(json).unwrap();
        assert_eq!(
            profile,
            AuthProfile::aws_sigv4("AKIDEXAMPLE", "secret", None, "us-east-1")
        );
        let creds = profile.aws_sigv4_value().unwrap();
        assert_eq!(creds.access_key, "AKIDEXAMPLE");
        assert_eq!(creds.region, "us-east-1");
        assert_eq!(creds.session_token, None);
        assert_eq!(profile.api_key_value(), None);
        assert_eq!(AuthProfile::api_key("k").aws_sigv4_value(), None);
    }

    #[test]
    fn parse_aws_sigv4_profile_with_session_token() {
        let json = r#"{
            "type": "aws_sigv4",
            "access_key": "AKIDEXAMPLE",
            "secret_key": "secret",
            "session_token": "token",
            "region": "eu-west-1"
        }"#;
        let profile: AuthProfile = serde_json::from_str(json).unwrap();
        let creds = profile.aws_sigv4_value().unwrap();
        assert_eq!(creds.session_token, Some("token".to_string()));
    }

    #[test]
    fn load_resolves_env_var_in_api_key() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"profiles": {{"openai-default": {{"type": "api_key", "key": "${{TEST_API_KEY}}"}}}}}}"#
        )
        .unwrap();

        // SAFETY: single-threaded test; no other code reads this variable.
        unsafe { std::env::set_var("TEST_API_KEY", "resolved-key") };
        let contents = std::fs::read_to_string(file.path()).unwrap();
        let raw: AuthProfilesFile = serde_json::from_str(&contents).unwrap();
        let resolved = resolve_env_vars_in_value(&raw.profiles["openai-default"]).unwrap();
        let profile: AuthProfile = serde_json::from_value(resolved).unwrap();

        assert_eq!(profile.api_key_value(), Some("resolved-key"));
        unsafe { std::env::remove_var("TEST_API_KEY") };
    }

    #[test]
    fn resolve_env_var_uses_default() {
        let value = serde_json::Value::String("${MISSING_VAR:default-value}".to_string());
        let resolved = resolve_env_vars_in_value(&value).unwrap();
        assert_eq!(
            resolved,
            serde_json::Value::String("default-value".to_string())
        );
    }

    #[test]
    fn load_auth_profiles_reads_from_home_dir() {
        let dir = tempfile::tempdir().unwrap();
        let profile_dir = dir.path().join(".legion/agents/test-agent/agent");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(
            profile_dir.join("auth-profiles.json"),
            r#"{
                "profiles": {
                    "openai-default": { "type": "api_key", "key": "sk-literal" },
                    "minimax-default": { "type": "api_key", "key": "${LEGION_TEST_VAR_HOME_LOAD}" }
                }
            }"#,
        )
        .unwrap();

        let old_home = std::env::var_os("HOME");
        // SAFETY: uniquely-named variable plus a HOME override; no other test
        // in this binary reads HOME, and both are restored right after the
        // load, before any assertion can panic.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::set_var("LEGION_TEST_VAR_HOME_LOAD", "sk-expanded");
        }
        let loaded = load_auth_profiles("test-agent");
        unsafe {
            match old_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
            std::env::remove_var("LEGION_TEST_VAR_HOME_LOAD");
        }

        let profiles = loaded.unwrap();
        assert_eq!(
            profiles.get("openai-default").unwrap().api_key_value(),
            Some("sk-literal")
        );
        assert_eq!(
            profiles.get("minimax-default").unwrap().api_key_value(),
            Some("sk-expanded")
        );
    }

    #[test]
    fn resolve_env_var_missing_without_default_errors() {
        // SAFETY: uniquely-named variable for this test; removed to guarantee
        // it is unset even if the developer's shell defines it.
        unsafe { std::env::remove_var("LEGION_TEST_VAR_UNSET_NO_DEFAULT") };
        let value = serde_json::Value::String("${LEGION_TEST_VAR_UNSET_NO_DEFAULT}".to_string());
        let err = resolve_env_vars_in_value(&value).unwrap_err();
        match err {
            ProviderError::InvalidAuth(msg) => {
                assert!(msg.contains("LEGION_TEST_VAR_UNSET_NO_DEFAULT"));
            }
            other => panic!("expected InvalidAuth, got {other}"),
        }
    }
}
