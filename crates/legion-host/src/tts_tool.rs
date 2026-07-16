//! `tts` tool (tools-p1p2 gap, Phase C).
//!
//! Synthesizes speech through the shared `ProviderRouter` (same pattern as
//! `image_tool.rs`: the tool lives in the host because it needs the
//! host-owned router). The resulting audio is written under
//! `<workspace>/generated/` and the file path is returned to the model.
//!
//! Scope (gap doc §4.5): channel-side voice delivery and the voice channel
//! capabilities gate are a later slice — this version only materializes the
//! audio file in the workspace and returns its path. Policy defaults to
//! `Approval::Off` (low risk).

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use legion_provider::router::ProviderRouter;
use legion_provider::types::SpeechRequest;
use legion_runtime::tools::{Policy, Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

/// Default model reference when the call omits `model`. Uses the explicit
/// `provider/model` form because bare model names only resolve through the
/// alias table.
const DEFAULT_TTS_MODEL: &str = "openai/tts-1";

/// Fallback container format when neither the call nor the provider
/// response specifies one.
const DEFAULT_TTS_FORMAT: &str = "mp3";

/// Write synthesized audio to `<workspace>/generated/tts-<millis>.<format>`
/// and return the file path.
///
/// The format is sanitized to ASCII alphanumerics so a hostile or buggy
/// provider response cannot inject path separators into the file name.
pub async fn write_audio(workspace: &Path, audio: &[u8], format: &str) -> Result<String, String> {
    let dir = workspace.join("generated");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let ext: String = format
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let ext = if ext.is_empty() {
        DEFAULT_TTS_FORMAT
    } else {
        &ext
    };

    let path = dir.join(format!("tts-{timestamp}.{ext}"));
    tokio::fs::write(&path, audio)
        .await
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// `tts`: text-to-speech via the routed provider.
pub struct TtsTool {
    router: Arc<ProviderRouter>,
    policy: Policy,
}

impl TtsTool {
    pub fn new(router: Arc<ProviderRouter>, policy: Policy) -> Self {
        Self { router, policy }
    }
}

#[async_trait]
impl Tool for TtsTool {
    fn name(&self) -> &str {
        "tts"
    }

    fn description(&self) -> &str {
        "Synthesize speech from text using the configured speech model. \
         Returns the path to the audio file written under the workspace's \
         generated/ directory."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Text to synthesize into speech."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model reference (provider/model). Defaults to openai/tts-1."
                },
                "voice": {
                    "type": "string",
                    "description": "Optional provider voice name, e.g. 'alloy' or 'nova'."
                },
                "format": {
                    "type": "string",
                    "description": "Optional audio container format, e.g. 'mp3' or 'opus'."
                }
            },
            "required": ["text"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'text' parameter".to_string()))?;
        if text.trim().is_empty() {
            return Err(ToolError::InvalidParams(
                "'text' must not be empty".to_string(),
            ));
        }
        tracing::info!(tool = "tts", text_len = text.len(), "tts tool invoked");

        let model_ref = input
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_TTS_MODEL);
        let voice = input
            .get("voice")
            .and_then(Value::as_str)
            .map(str::to_string);
        let format = input
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_string);

        let request = SpeechRequest {
            model: String::new(), // filled by the router per fallback candidate
            input: text.to_string(),
            voice,
            format,
        };
        let response = self
            .router
            .synthesize_speech(model_ref, request)
            .await
            .map_err(|e| ToolError::Execution(format!("speech synthesis failed: {e}")))?;

        let path = write_audio(&ctx.workspace, &response.audio, &response.format)
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolResult::ok(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(dir: &TempDir) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: None,
            swarm: None,
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
            background_tasks: None,
        }
    }

    fn open_policy() -> Policy {
        Policy {
            approval: legion_runtime::tools::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    #[tokio::test]
    async fn write_audio_materializes_file_under_generated() {
        let dir = TempDir::new().unwrap();
        let path = write_audio(dir.path(), b"fake-mp3", "mp3").await.unwrap();

        let path = std::path::PathBuf::from(&path);
        assert!(path.starts_with(dir.path().join("generated")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp3"));
        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"fake-mp3");
    }

    #[tokio::test]
    async fn write_audio_sanitizes_format_extension() {
        let dir = TempDir::new().unwrap();
        let path = write_audio(dir.path(), b"x", "../evil").await.unwrap();
        let path = std::path::PathBuf::from(&path);
        // Path separators must be stripped, keeping the file inside generated/.
        assert!(path.starts_with(dir.path().join("generated")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("evil"));
    }

    #[tokio::test]
    async fn write_audio_empty_format_falls_back_to_default() {
        let dir = TempDir::new().unwrap();
        let path = write_audio(dir.path(), b"x", "").await.unwrap();
        let path = std::path::PathBuf::from(&path);
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some(DEFAULT_TTS_FORMAT)
        );
    }

    #[test]
    fn schema_requires_text() {
        let tool = TtsTool::new(Arc::new(ProviderRouter::new()), open_policy());
        let schema = tool.schema();
        assert_eq!(schema["required"], json!(["text"]));
        assert!(schema["properties"]["voice"].is_object());
        assert!(schema["properties"]["format"].is_object());
    }

    #[tokio::test]
    async fn execute_rejects_missing_or_empty_text() {
        let dir = TempDir::new().unwrap();
        let tool = TtsTool::new(Arc::new(ProviderRouter::new()), open_policy());

        let err = tool.execute(json!({}), ctx(&dir)).await.unwrap_err();
        assert!(err.to_string().contains("missing 'text'"));

        let err = tool
            .execute(json!({"text": "   "}), ctx(&dir))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }
}
