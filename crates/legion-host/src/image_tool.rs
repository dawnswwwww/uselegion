//! `image_generate` tool (tools-p1p2 gap, Phase B).
//!
//! Generates images through the shared `ProviderRouter` (same pattern as the
//! session tools in `session_tools.rs`: the tool lives in the host because
//! it needs host-owned state). Results arrive either as hosted URLs
//! (returned verbatim) or as base64-encoded PNG data, which is decoded and
//! written under `<workspace>/generated/`.
//!
//! Safety (gap doc §4.3/§6.1): the tool defaults to `Approval::Required`
//! (cost + content risk) and applies a local keyword precheck before any
//! provider call. The precheck is a deliberately tiny heuristic coarse
//! filter, not a real content moderator — provider-side moderation remains
//! the primary defense.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use legion_provider::router::ProviderRouter;
use legion_provider::types::{GeneratedImage, ImageRequest};
use legion_runtime::tools::{Policy, Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

/// Default model reference when the call omits `model`. Uses the explicit
/// `provider/model` form because bare model names only resolve through the
/// alias table.
const DEFAULT_IMAGE_MODEL: &str = "openai/dall-e-3";

/// Tiny hard-coded blocklist for the local prompt precheck. Heuristic coarse
/// filter only (gap doc §6.1): substring-matched against the lowercased
/// prompt before any provider call is made.
const BLOCKED_KEYWORDS: &[&str] = &["nude", "gore", "child abuse"];

/// Local content precheck: reject prompts containing a blocked keyword.
pub fn precheck_prompt(prompt: &str) -> Result<(), String> {
    let lower = prompt.to_lowercase();
    for keyword in BLOCKED_KEYWORDS {
        if lower.contains(keyword) {
            return Err("prompt rejected by local content precheck".to_string());
        }
    }
    Ok(())
}

/// Materialize provider results into returnable references.
///
/// `b64_json` images are decoded and written to `<workspace>/generated/`
/// (created on demand); `url` images pass through unchanged. Every image
/// yields exactly one line: a path, a URL, or an `error: ...` line, so a
/// single bad image does not fail the whole batch.
pub async fn materialize_images(workspace: &Path, images: &[GeneratedImage]) -> Vec<String> {
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

/// `image_generate`: text-to-image via the routed provider.
pub struct ImageGenerateTool {
    router: Arc<ProviderRouter>,
    policy: Policy,
}

impl ImageGenerateTool {
    pub fn new(router: Arc<ProviderRouter>, policy: Policy) -> Self {
        Self { router, policy }
    }
}

#[async_trait]
impl Tool for ImageGenerateTool {
    fn name(&self) -> &str {
        "image_generate"
    }

    fn description(&self) -> &str {
        "Generate one or more images from a text prompt using the configured \
         image model. Returns hosted URLs and/or paths to PNG files written \
         under the workspace's generated/ directory. Costs money and is \
         subject to a local content precheck."
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
                    "description": "Text description of the image to generate."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model reference (provider/model). Defaults to openai/dall-e-3."
                },
                "size": {
                    "type": "string",
                    "description": "Optional image size, e.g. '1024x1024'."
                }
            },
            "required": ["prompt"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidParams("missing 'prompt' parameter".to_string()))?;

        precheck_prompt(prompt).map_err(ToolError::Execution)?;

        let model_ref = input
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_IMAGE_MODEL);
        let size = input
            .get("size")
            .and_then(Value::as_str)
            .map(str::to_string);

        let request = ImageRequest {
            model: String::new(), // filled by the router per fallback candidate
            prompt: prompt.to_string(),
            size,
            n: None,
        };
        let response = self
            .router
            .generate_image(model_ref, request)
            .await
            .map_err(|e| ToolError::Execution(format!("image generation failed: {e}")))?;

        if response.images.is_empty() {
            return Ok(ToolResult::error("provider returned no images"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn precheck_accepts_clean_prompt() {
        assert!(precheck_prompt("a watercolor fox in a snowy forest").is_ok());
    }

    #[test]
    fn precheck_rejects_blocked_keywords_case_insensitively() {
        for prompt in [
            "draw a NUDE figure",
            "graphic GORE scene",
            "child abuse imagery",
        ] {
            let err = precheck_prompt(prompt).expect_err("must be rejected");
            assert!(
                err.contains("prompt rejected by local content precheck"),
                "unexpected message: {err}"
            );
        }
    }

    #[tokio::test]
    async fn materialize_writes_b64_png_to_workspace() {
        let dir = TempDir::new().unwrap();
        let images = vec![GeneratedImage {
            url: None,
            b64_json: Some(STANDARD.encode(b"png-bytes")),
        }];
        let lines = materialize_images(dir.path(), &images).await;

        assert_eq!(lines.len(), 1);
        let path = std::path::PathBuf::from(&lines[0]);
        assert!(path.starts_with(dir.path().join("generated")));
        assert!(path.extension().is_some_and(|ext| ext == "png"));
        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, b"png-bytes");
    }

    #[tokio::test]
    async fn materialize_passes_urls_through() {
        let dir = TempDir::new().unwrap();
        let images = vec![GeneratedImage {
            url: Some("https://example.com/cat.png".to_string()),
            b64_json: None,
        }];
        let lines = materialize_images(dir.path(), &images).await;
        assert_eq!(lines, vec!["https://example.com/cat.png".to_string()]);
        // URL-only results must not create the generated/ directory.
        assert!(!dir.path().join("generated").exists());
    }

    #[tokio::test]
    async fn materialize_reports_bad_image_without_failing_batch() {
        let dir = TempDir::new().unwrap();
        let images = vec![
            GeneratedImage {
                url: None,
                b64_json: Some("!!! not base64 !!!".to_string()),
            },
            GeneratedImage {
                url: Some("https://example.com/ok.png".to_string()),
                b64_json: None,
            },
            GeneratedImage {
                url: None,
                b64_json: None,
            },
        ];
        let lines = materialize_images(dir.path(), &images).await;

        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("error: invalid base64"),
            "{}",
            lines[0]
        );
        assert_eq!(lines[1], "https://example.com/ok.png");
        assert!(lines[2].starts_with("error:"), "{}", lines[2]);
    }
}
