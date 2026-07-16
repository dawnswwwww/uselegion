//! `video_generate` tool: generate a video from a text prompt and optional
//! image reference.
//!
//! The current implementation delegates to `ProviderRouter::generate_video`. If
//! no provider in the chain supports video generation, the tool returns a
//! structured error result.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{GeneratedVideo, VideoRequest};
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde_json::{Value, json};

use crate::policy::Policy;

const DEFAULT_VIDEO_MODEL: &str = "openai/sora";

/// Generate a video from a text prompt.
pub struct VideoGenerateTool {
    router: Arc<ProviderRouter>,
    policy: Policy,
}

impl VideoGenerateTool {
    pub fn new(router: Arc<ProviderRouter>, policy: Policy) -> Self {
        Self { router, policy }
    }
}

#[async_trait]
impl Tool for VideoGenerateTool {
    fn name(&self) -> &str {
        "video_generate"
    }

    fn description(&self) -> &str {
        "Generate a video from a text prompt. Optionally use an image as the first frame."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text description of the video to generate"
                },
                "imagePath": {
                    "type": "string",
                    "description": "Optional path to an image to use as the first frame"
                },
                "duration": {
                    "type": "integer",
                    "description": "Optional desired duration in seconds"
                }
            },
            "required": ["prompt"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn kind(&self) -> ToolKind {
        ToolKind::VideoGen
    }

    fn namespace(&self) -> ToolNamespace {
        ToolNamespace::Legion
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;
        let image_path = input
            .get("imagePath")
            .and_then(Value::as_str)
            .map(str::to_string);
        let duration = input
            .get("duration")
            .and_then(Value::as_u64)
            .map(|v| v as u32);

        let model_ref = input
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_VIDEO_MODEL);

        let request = VideoRequest {
            model: String::new(),
            prompt: prompt.to_string(),
            image_path,
            duration,
        };

        let response = self
            .router
            .generate_video(model_ref, request)
            .await
            .map_err(|e| {
                ToolError::Execution(format!(
                    "video_generate is not supported by the configured providers: {e}"
                ))
            })?;

        if response.videos.is_empty() {
            return Ok(ToolResult::error("provider returned no videos"));
        }

        let lines = materialize_videos(&ctx.workspace, &response.videos).await;
        let content = lines.join("\n");
        if lines.iter().all(|line| line.starts_with("error:")) {
            Ok(ToolResult::error(content))
        } else {
            Ok(ToolResult::ok(content))
        }
    }
}

async fn materialize_videos(workspace: &Path, videos: &[GeneratedVideo]) -> Vec<String> {
    let dir = workspace.join("generated");
    let mut lines = Vec::with_capacity(videos.len());
    for (i, video) in videos.iter().enumerate() {
        if let Some(url) = &video.url {
            lines.push(url.clone());
        } else if let Some(path) = &video.path {
            lines.push(path.clone());
        } else {
            lines.push(format!("error: video {i} carries neither url nor path"));
        }
    }

    if lines.iter().any(|l| l.starts_with("error:")) {
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            lines.push(format!("error: failed to create {}: {e}", dir.display()));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Approval;
    use tempfile::TempDir;

    fn policy() -> Policy {
        Policy {
            approval: Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

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
            plan_mode_tracker: None,
        }
    }

    #[test]
    fn schema_requires_prompt() {
        let router = Arc::new(ProviderRouter::new());
        let tool = VideoGenerateTool::new(router, policy());
        let schema = tool.schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("prompt")));
    }

    #[tokio::test]
    async fn no_provider_support_returns_structured_error() {
        let dir = TempDir::new().unwrap();
        let router = Arc::new(ProviderRouter::new());
        let tool = VideoGenerateTool::new(router, policy());
        let res = tool.execute(json!({"prompt": "a cat"}), ctx(&dir)).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("video_generate is not supported"));
    }
}
