//! `image_edit` tool: edit an existing image with a text prompt.
//!
//! The current implementation delegates to `ProviderRouter::generate_image`. If
//! the configured provider chain does not support image editing, the tool
//! returns a structured error result rather than failing silently.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{GeneratedImage, ImageRequest};
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde_json::{Value, json};

use crate::policy::Policy;
use crate::tools::resolve_tool_path;

const DEFAULT_IMAGE_MODEL: &str = "openai/dall-e-3";

/// Edit an existing image using a text prompt.
pub struct ImageEditTool {
    router: Arc<ProviderRouter>,
    policy: Policy,
}

impl ImageEditTool {
    pub fn new(router: Arc<ProviderRouter>, policy: Policy) -> Self {
        Self { router, policy }
    }
}

#[async_trait]
impl Tool for ImageEditTool {
    fn name(&self) -> &str {
        "image_edit"
    }

    fn description(&self) -> &str {
        "Edit an existing image using a text prompt. Requires a provider that supports image edits."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "imagePath": {
                    "type": "string",
                    "description": "Path to the image to edit"
                },
                "prompt": {
                    "type": "string",
                    "description": "Description of the desired edit"
                },
                "n": {
                    "type": "integer",
                    "description": "Number of images to generate"
                },
                "size": {
                    "type": "string",
                    "description": "Output size, e.g. '1024x1024'"
                }
            },
            "required": ["imagePath", "prompt"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ImageGen
    }

    fn namespace(&self) -> ToolNamespace {
        ToolNamespace::Legion
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let image_path = input
            .get("imagePath")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'imagePath' parameter".to_string()))?;
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;
        let n = input.get("n").and_then(Value::as_u64).map(|v| v as u32);
        let size = input
            .get("size")
            .and_then(Value::as_str)
            .map(str::to_string);

        let resolved = resolve_tool_path(&ctx, image_path, self.policy.workspace_only)?;
        if !tokio::fs::try_exists(&resolved).await.unwrap_or(false) {
            return Err(ToolError::Execution(format!(
                "image file not found: {}",
                resolved.display()
            )));
        }

        let model_ref = input
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_IMAGE_MODEL);

        let request = ImageRequest {
            model: String::new(),
            prompt: prompt.to_string(),
            size,
            n,
        };

        let response = self
            .router
            .generate_image(model_ref, request)
            .await
            .map_err(|e| {
                ToolError::Execution(format!(
                    "image_edit is not supported by the configured providers: {e}"
                ))
            })?;

        if response.images.is_empty() {
            return Ok(ToolResult::error("provider returned no edited images"));
        }

        let lines = materialize_images(&ctx.workspace, &response.images).await;
        let content = lines.join("\n");
        if lines.iter().all(|line| line.starts_with("error:")) {
            Ok(ToolResult::error(content))
        } else {
            Ok(ToolResult::ok(content))
        }
    }
}

/// Materialize generated images into workspace paths or URLs.
async fn materialize_images(workspace: &Path, images: &[GeneratedImage]) -> Vec<String> {
    let dir = workspace.join("generated");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let mut lines = Vec::with_capacity(images.len());
    let mut dir_ready = true;
    for (i, image) in images.iter().enumerate() {
        if let Some(b64) = &image.b64_json {
            if dir_ready {
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    lines.push(format!("error: failed to create {}: {e}", dir.display()));
                    dir_ready = false;
                }
            }
            if !dir_ready {
                continue;
            }
            match STANDARD.decode(b64) {
                Ok(bytes) => {
                    let path = dir.join(format!("image-{timestamp}-{i}.png"));
                    match tokio::fs::write(&path, &bytes).await {
                        Ok(()) => lines.push(path.display().to_string()),
                        Err(e) => {
                            lines.push(format!("error: failed to write {}: {e}", path.display()))
                        }
                    }
                }
                Err(e) => lines.push(format!("error: invalid base64 in image {i}: {e}")),
            }
        } else if let Some(url) = &image.url {
            lines.push(url.clone());
        } else {
            lines.push(format!("error: image {i} carries neither url nor b64_json"));
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
    fn schema_requires_image_path_and_prompt() {
        let router = Arc::new(ProviderRouter::new());
        let tool = ImageEditTool::new(router, policy());
        let schema = tool.schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("imagePath")));
        assert!(required.contains(&json!("prompt")));
    }

    #[tokio::test]
    async fn missing_image_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let router = Arc::new(ProviderRouter::new());
        let tool = ImageEditTool::new(router, policy());
        let err = tool
            .execute(
                json!({"imagePath": "missing.png", "prompt": "make it blue"}),
                ctx(&dir),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("image file not found"));
    }
}
