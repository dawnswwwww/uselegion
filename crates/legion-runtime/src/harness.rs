use async_trait::async_trait;
use std::sync::Arc;

use crate::AgentRuntime;
use crate::types::{RunRequest, RunStream, RuntimeError};

/// A harness executes an agent run.
///
/// Implementations may use the built-in loop or delegate to an external
/// ACP-compatible harness process.
#[async_trait]
pub trait Harness: Send + Sync {
    /// Stable harness identifier (e.g. `"built-in"`, `"acp"`).
    fn id(&self) -> &str;

    /// Returns `true` when this harness should handle the given model ref.
    fn can_handle(&self, model_ref: &str) -> bool;

    /// Start an agent run and return a stream of runtime events.
    fn run(&self, request: RunRequest) -> Result<RunStream, RuntimeError>;
}

#[async_trait]
impl Harness for AgentRuntime {
    fn id(&self) -> &str {
        "built-in"
    }

    fn can_handle(&self, model_ref: &str) -> bool {
        !model_ref.starts_with("acp:")
    }

    fn run(&self, request: RunRequest) -> Result<RunStream, RuntimeError> {
        AgentRuntime::run(self, request)
    }
}

/// Registry that selects a harness based on configuration or model ref.
#[derive(Default)]
pub struct HarnessRegistry {
    harnesses: Vec<Arc<dyn Harness>>,
    default_id: Option<String>,
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the harness id that should be used regardless of model ref.
    pub fn with_default(mut self, id: impl Into<String>) -> Self {
        self.default_id = Some(id.into());
        self
    }

    /// Register a harness.
    pub fn register(&mut self, harness: Arc<dyn Harness>) {
        self.harnesses.push(harness);
    }

    /// Select a harness for the given model ref.
    pub fn select(&self, model_ref: &str) -> Option<Arc<dyn Harness>> {
        if let Some(id) = &self.default_id {
            if let Some(h) = self.harnesses.iter().find(|h| h.id() == id) {
                return Some(h.clone());
            }
        }
        self.harnesses
            .iter()
            .find(|h| h.can_handle(model_ref))
            .cloned()
    }

    /// Run a request using the selected harness.
    pub fn run(&self, request: RunRequest) -> Result<RunStream, RuntimeError> {
        let harness = self.select(&request.model_ref).ok_or_else(|| {
            RuntimeError::Context(format!(
                "no harness available for model {}",
                request.model_ref
            ))
        })?;
        harness.run(request)
    }
}

#[async_trait]
impl Harness for HarnessRegistry {
    fn id(&self) -> &str {
        "registry"
    }

    fn can_handle(&self, _model_ref: &str) -> bool {
        true
    }

    fn run(&self, request: RunRequest) -> Result<RunStream, RuntimeError> {
        HarnessRegistry::run(self, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyHarness {
        id: String,
        handles: Vec<String>,
    }

    #[async_trait]
    impl Harness for DummyHarness {
        fn id(&self) -> &str {
            &self.id
        }

        fn can_handle(&self, model_ref: &str) -> bool {
            self.handles.iter().any(|h| model_ref.starts_with(h))
        }

        fn run(&self, _request: RunRequest) -> Result<RunStream, RuntimeError> {
            Ok(Box::pin(futures::stream::iter(Vec::new())))
        }
    }

    #[test]
    fn registry_selects_by_model_ref() {
        let mut registry = HarnessRegistry::new();
        registry.register(Arc::new(DummyHarness {
            id: "built-in".to_string(),
            handles: vec!["openai/".to_string()],
        }));
        registry.register(Arc::new(DummyHarness {
            id: "acp".to_string(),
            handles: vec!["acp:".to_string()],
        }));

        assert_eq!(registry.select("openai/gpt-4o").unwrap().id(), "built-in");
        assert_eq!(registry.select("acp:mock").unwrap().id(), "acp");
        assert!(registry.select("unknown/model").is_none());
    }

    #[test]
    fn registry_default_overrides_model_ref() {
        let mut registry = HarnessRegistry::new();
        registry.register(Arc::new(DummyHarness {
            id: "built-in".to_string(),
            handles: vec!["openai/".to_string()],
        }));
        registry.register(Arc::new(DummyHarness {
            id: "acp".to_string(),
            handles: vec!["acp:".to_string()],
        }));
        let registry = registry.with_default("acp");

        assert_eq!(registry.select("openai/gpt-4o").unwrap().id(), "acp");
        assert_eq!(registry.select("acp:mock").unwrap().id(), "acp");
    }
}
