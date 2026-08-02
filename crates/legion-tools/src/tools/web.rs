use async_trait::async_trait;
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde_json::json;

use crate::policy::Policy;

/// Helper to stamp `kind()` and `namespace()` on a built-in Legion tool.
macro_rules! legion_tool_taxonomy {
    ($kind:expr) => {
        fn kind(&self) -> ToolKind {
            $kind
        }
        fn namespace(&self) -> ToolNamespace {
            ToolNamespace::Legion
        }
    };
}

/// Fetch a single web page and return its main text content.
pub struct WebFetchTool {
    pub policy: Policy,
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a single URL and return the stripped text content."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch" }
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    legion_tool_taxonomy!(ToolKind::WebFetch);

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let url = params["url"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'url' parameter".to_string()))?;

        let body = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("request failed: {}", e)))?
            .text()
            .await
            .map_err(|e| ToolError::Execution(format!("failed to read response: {}", e)))?;

        let text = strip_html(&body);
        Ok(ToolResult::ok(text))
    }
}

/// Strip HTML tags and decode common entities into plain text.
pub(super) fn strip_html(html: &str) -> String {
    let tag_re = regex::Regex::new(r"<[^>]+>").unwrap();
    let mut text = tag_re.replace_all(html, " ").to_string();

    let entities = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&nbsp;", " "),
    ];
    for (ent, ch) in entities {
        text = text.replace(ent, ch);
    }

    // Collapse whitespace.
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::{MockServer, ResponseTemplate};

    fn ctx(dir: &TempDir, sender: Option<&str>) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            sender: sender.map(|s| s.to_string()),
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

    fn open_policy() -> Policy {
        Policy {
            approval: crate::policy::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    #[test]
    fn strip_html_works() {
        let html = "<p>Hello &amp; <b>world</b></p>";
        assert_eq!(strip_html(html), "Hello & world");
    }

    #[tokio::test]
    async fn web_fetch_with_wiremock() {
        let server = MockServer::start().await;
        let body = "<html><body><p>Hello world</p></body></html>";
        server
            .register(
                wiremock::Mock::given(method("GET"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(body)),
            )
            .await;

        let dir = TempDir::new().unwrap();
        let tool = WebFetchTool::new(open_policy());
        let res = tool
            .execute(json!({"url": server.uri()}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(res.content.contains("Hello world"));
    }

    #[test]
    fn web_fetch_is_read_only_and_concurrency_safe() {
        let fetch = WebFetchTool::new(open_policy());
        assert!(fetch.is_read_only(&json!({"url": "http://x"})));
        assert!(fetch.is_concurrency_safe(&json!({"url": "http://x"})));
    }
}
