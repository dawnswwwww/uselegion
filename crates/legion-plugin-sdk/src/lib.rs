pub mod channel;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub use channel::{ChannelProvider, InboundMessage, OutboundMessage, Peer, Sender};
pub use legion_skills::{Skill, SkillSource};

/// Metadata describing a plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginMetadata {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: PluginKind,
    pub description: Option<String>,
}

/// The kind of plugin extension point.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum PluginKind {
    Channel,
    Tool,
    Memory,
    ContextEngine,
    Harness,
    CliBackend,
    Diagnostics,
    Skill,
}

/// A boxed, dynamically dispatched plugin.
pub type BoxedPlugin = Box<dyn Plugin + Send + Sync>;

/// The core trait implemented by every plugin.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn metadata(&self) -> PluginMetadata;

    /// Capabilities this plugin provides.
    fn capabilities(&self) -> Vec<Capability> {
        vec![self.metadata().kind.into()]
    }

    /// Called once when the plugin is loaded into the Gateway.
    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        Ok(PluginHandles::default())
    }

    /// Called when the Gateway is shutting down.
    async fn shutdown(&self) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PluginError {
    #[error("plugin '{0}' is already registered")]
    AlreadyRegistered(String),
    #[error("plugin '{0}' not found")]
    NotFound(String),
    #[error("channel '{0}' not provided by any plugin")]
    ChannelNotFound(String),
    #[error("invalid plugin configuration: {0}")]
    InvalidConfig(String),
    #[error("plugin '{0}' initialization failed: {1}")]
    InitFailed(String, String),
}

/// Capabilities a plugin may provide.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Channel,
    Tool,
    Memory,
    ContextEngine,
    Harness,
    CliBackend,
    Diagnostics,
    Skill,
}

impl From<PluginKind> for Capability {
    fn from(kind: PluginKind) -> Self {
        match kind {
            PluginKind::Channel => Capability::Channel,
            PluginKind::Tool => Capability::Tool,
            PluginKind::Memory => Capability::Memory,
            PluginKind::ContextEngine => Capability::ContextEngine,
            PluginKind::Harness => Capability::Harness,
            PluginKind::CliBackend => Capability::CliBackend,
            PluginKind::Diagnostics => Capability::Diagnostics,
            PluginKind::Skill => Capability::Skill,
        }
    }
}

/// Context passed to a plugin during initialization.
#[derive(Debug, Clone, Default)]
pub struct PluginContext {
    /// Plugin-specific configuration.
    pub config: serde_json::Value,
    /// Gateway workspace directory.
    pub workspace: PathBuf,
    /// Optional agent scope. When `None`, the plugin is global.
    pub agent_id: Option<String>,
}

/// Handles returned by a plugin after initialization. The registry distributes
/// them to the appropriate Gateway subsystems.
#[derive(Default, Clone)]
pub struct PluginHandles {
    pub channels: Vec<Arc<dyn ChannelProvider>>,
    pub skills: Vec<Skill>,
}

impl std::fmt::Debug for PluginHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHandles")
            .field("channels", &self.channels.len())
            .field("skills", &self.skills.len())
            .finish()
    }
}

/// Lifecycle status of a loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    Loaded,
    Initialized,
    Failed(String),
    Disabled,
}

/// Manifest describing a user or dynamic-library plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub library: Option<std::path::PathBuf>,
    /// Paths to SKILL.md files shipped by this manifest plugin, resolved
    /// relative to the manifest directory.
    #[serde(default)]
    pub skills: Vec<std::path::PathBuf>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub min_legion_version: Option<String>,
    #[serde(default)]
    pub config: serde_json::Value,
}

/// A plugin backed solely by a manifest (no dynamic library). It declares
/// capabilities and can carry configuration. It may also ship SKILL.md files
/// listed in `manifest.skills`.
#[derive(Debug, Clone)]
pub struct ManifestPlugin {
    pub manifest: PluginManifest,
    /// Directory containing the manifest, used to resolve relative skill paths.
    pub manifest_dir: PathBuf,
}

impl ManifestPlugin {
    pub fn new(manifest: PluginManifest, manifest_dir: PathBuf) -> Self {
        Self {
            manifest,
            manifest_dir,
        }
    }
}

#[async_trait]
impl Plugin for ManifestPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: self.manifest.id.clone(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            // Use Channel as a fallback for metadata kind when multiple or no
            // capabilities are declared; the real capability list comes from
            // `capabilities()`.
            kind: match self.manifest.capabilities.first() {
                Some(Capability::Channel) => PluginKind::Channel,
                Some(Capability::Tool) => PluginKind::Tool,
                Some(Capability::Memory) => PluginKind::Memory,
                Some(Capability::ContextEngine) => PluginKind::ContextEngine,
                Some(Capability::Harness) => PluginKind::Harness,
                Some(Capability::CliBackend) => PluginKind::CliBackend,
                Some(Capability::Diagnostics) => PluginKind::Diagnostics,
                Some(Capability::Skill) => PluginKind::Skill,
                None => PluginKind::Channel,
            },
            description: None,
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.manifest.capabilities.clone()
    }

    async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
        if self.manifest.library.is_some() {
            return Err(PluginError::InitFailed(
                self.manifest.id.clone(),
                "dynamic library plugins are not supported in this build".to_string(),
            ));
        }

        let mut skills = Vec::new();
        for skill_path in &self.manifest.skills {
            let resolved = self.manifest_dir.join(skill_path);
            let content = std::fs::read_to_string(&resolved).map_err(|e| {
                PluginError::InitFailed(
                    self.manifest.id.clone(),
                    format!("cannot read skill {}: {e}", resolved.display()),
                )
            })?;
            let skill = legion_skills::parse_skill_md(&content, resolved, SkillSource::Plugin)
                .map_err(|e| PluginError::InitFailed(self.manifest.id.clone(), e.to_string()))?;
            skills.push(skill);
        }

        Ok(PluginHandles {
            channels: Vec::new(),
            skills,
        })
    }
}

/// Registry holding all loaded plugins, indexed by extension point.
#[derive(Default)]
pub struct PluginRegistry {
    plugins: Vec<BoxedPlugin>,
    channels: HashMap<String, Arc<dyn ChannelProvider>>,
    skills: Vec<Skill>,
    status: HashMap<String, PluginStatus>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a generic plugin.
    pub fn load(&mut self, plugin: BoxedPlugin) -> Result<(), PluginError> {
        let meta = plugin.metadata();
        if self.plugins.iter().any(|p| p.metadata().id == meta.id) {
            return Err(PluginError::AlreadyRegistered(meta.id));
        }
        self.status.insert(meta.id.clone(), PluginStatus::Loaded);
        self.plugins.push(plugin);
        Ok(())
    }

    /// Initialize all loaded plugins and collect the returned handles.
    pub async fn init_all(&mut self, ctx: &PluginContext) -> Result<(), PluginError> {
        // Collect ids first to avoid borrowing issues while iterating.
        let ids: Vec<String> = self
            .plugins
            .iter()
            .map(|p| p.metadata().id.clone())
            .collect();
        for plugin in &self.plugins {
            let id = plugin.metadata().id.clone();
            match plugin.init(ctx).await {
                Ok(handles) => {
                    let mut failed = false;
                    for channel in handles.channels {
                        let channel_id = channel.channel_id().to_string();
                        if self.channels.contains_key(&channel_id) {
                            self.status.insert(
                                id.clone(),
                                PluginStatus::Failed(format!(
                                    "channel '{}' already registered",
                                    channel_id
                                )),
                            );
                            failed = true;
                            continue;
                        }
                        self.channels.insert(channel_id, channel);
                    }
                    self.skills.extend(handles.skills);
                    // Don't overwrite a duplicate-channel failure recorded above.
                    if !failed {
                        self.status.insert(id, PluginStatus::Initialized);
                    }
                }
                Err(err) => {
                    self.status
                        .insert(id, PluginStatus::Failed(err.to_string()));
                }
            }
        }
        // Preserve ordering: ensure every loaded plugin has a status entry.
        for id in ids {
            self.status.entry(id).or_insert_with(|| {
                PluginStatus::Failed("plugin missing from init loop".to_string())
            });
        }
        Ok(())
    }

    /// Register a channel provider directly.
    pub fn register_channel(
        &mut self,
        channel_id: impl Into<String>,
        provider: Arc<dyn ChannelProvider>,
    ) -> Result<(), PluginError> {
        let channel_id = channel_id.into();
        if self.channels.contains_key(&channel_id) {
            return Err(PluginError::AlreadyRegistered(channel_id));
        }
        self.channels.insert(channel_id, provider);
        Ok(())
    }

    pub fn channel(&self, channel_id: &str) -> Option<Arc<dyn ChannelProvider>> {
        self.channels.get(channel_id).cloned()
    }

    pub fn channels(&self) -> &HashMap<String, Arc<dyn ChannelProvider>> {
        &self.channels
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn list(&self) -> &[BoxedPlugin] {
        &self.plugins
    }

    pub fn find(&self, id: &str) -> Option<&BoxedPlugin> {
        self.plugins.iter().find(|p| p.metadata().id == id)
    }

    pub fn status(&self) -> &HashMap<String, PluginStatus> {
        &self.status
    }

    /// Load all manifest plugins from `dir`, skipping any whose id appears in
    /// `disabled`. Manifest plugins are topologically sorted by `depends_on`
    /// before being loaded.
    pub fn load_dir(
        &mut self,
        dir: &std::path::Path,
        disabled: &[String],
    ) -> Result<(), PluginError> {
        let mut manifests: Vec<(std::path::PathBuf, PluginManifest)> = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|e| {
            PluginError::InvalidConfig(format!("cannot read plugin dir {}: {e}", dir.display()))
        })? {
            let entry = entry.map_err(|e| {
                PluginError::InvalidConfig(format!("cannot read plugin dir entry: {e}"))
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
                PluginError::InvalidConfig(format!(
                    "cannot read manifest {}: {e}",
                    manifest_path.display()
                ))
            })?;
            let manifest: PluginManifest = serde_json::from_str(&content).map_err(|e| {
                PluginError::InvalidConfig(format!(
                    "cannot parse manifest {}: {e}",
                    manifest_path.display()
                ))
            })?;
            if disabled.contains(&manifest.id) {
                continue;
            }
            manifests.push((path, manifest));
        }

        let sorted = topo_sort_manifests(&manifests)?;
        for (path, manifest) in sorted {
            self.load(Box::new(ManifestPlugin::new(manifest, path)))?;
        }
        Ok(())
    }
}

fn topo_sort_manifests(
    manifests: &[(std::path::PathBuf, PluginManifest)],
) -> Result<Vec<(std::path::PathBuf, PluginManifest)>, PluginError> {
    let by_id: HashMap<String, usize> = manifests
        .iter()
        .enumerate()
        .map(|(i, (_, m))| (m.id.clone(), i))
        .collect();

    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();

    for (_, manifest) in manifests {
        in_degree.entry(manifest.id.clone()).or_insert(0);
        for dep in &manifest.depends_on {
            if !by_id.contains_key(dep) {
                return Err(PluginError::InvalidConfig(format!(
                    "plugin '{}' depends on unknown plugin '{}'",
                    manifest.id, dep
                )));
            }
            dependents
                .entry(dep.clone())
                .or_default()
                .push(manifest.id.clone());
            *in_degree.entry(manifest.id.clone()).or_insert(0) += 1;
        }
    }

    let mut queue: Vec<String> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut sorted: Vec<(std::path::PathBuf, PluginManifest)> = Vec::new();

    while let Some(id) = queue.pop() {
        let idx = by_id[&id];
        sorted.push((manifests[idx].0.clone(), manifests[idx].1.clone()));
        for dependent in dependents.get(&id).cloned().unwrap_or_default() {
            let deg = in_degree.get_mut(&dependent).expect("in_degree entry");
            *deg -= 1;
            if *deg == 0 {
                queue.push(dependent);
            }
        }
    }

    if sorted.len() != manifests.len() {
        return Err(PluginError::InvalidConfig(
            "circular plugin dependency detected".to_string(),
        ));
    }

    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use channel::{ChannelCapabilities, ChannelError};

    struct DummyPlugin {
        id: String,
        kind: PluginKind,
    }

    #[async_trait]
    impl Plugin for DummyPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata {
                id: self.id.clone(),
                name: format!("{} plugin", self.id),
                version: "0.1.0".to_string(),
                kind: self.kind.clone(),
                description: None,
            }
        }
    }

    #[test]
    fn should_register_plugin() {
        let mut registry = PluginRegistry::new();
        let plugin = DummyPlugin {
            id: "test".to_string(),
            kind: PluginKind::Channel,
        };

        registry.load(Box::new(plugin)).unwrap();

        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.list()[0].metadata().id, "test");
    }

    #[test]
    fn should_reject_duplicate_plugin_id() {
        let mut registry = PluginRegistry::new();
        let p1 = DummyPlugin {
            id: "dup".to_string(),
            kind: PluginKind::Channel,
        };
        let p2 = DummyPlugin {
            id: "dup".to_string(),
            kind: PluginKind::Tool,
        };

        registry.load(Box::new(p1)).unwrap();
        let result = registry.load(Box::new(p2));

        assert_eq!(
            result,
            Err(PluginError::AlreadyRegistered("dup".to_string()))
        );
    }

    struct FakeChannelProvider;

    #[async_trait]
    impl ChannelProvider for FakeChannelProvider {
        fn channel_id(&self) -> &str {
            "fake"
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                text: true,
                media: vec![],
                group: true,
                thread: false,
                reactions: false,
                typing: false,
            }
        }

        async fn start(
            &self,
            _config: serde_json::Value,
            _inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
        ) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn stop(&self) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn send(&self, _message: OutboundMessage) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn should_register_and_lookup_channel_provider() {
        let mut registry = PluginRegistry::new();
        registry
            .register_channel("fake", Arc::new(FakeChannelProvider))
            .unwrap();

        let provider = registry.channel("fake").expect("channel should exist");
        assert_eq!(provider.channel_id(), "fake");
        assert!(provider.capabilities().text);
        assert!(provider.capabilities().group);
        assert!(!provider.capabilities().thread);
    }

    #[test]
    fn should_return_none_for_unknown_channel() {
        let registry = PluginRegistry::new();
        assert!(registry.channel("unknown").is_none());
    }

    #[test]
    fn should_reject_duplicate_channel_id() {
        let mut registry = PluginRegistry::new();
        registry
            .register_channel("fake", Arc::new(FakeChannelProvider))
            .unwrap();
        let result = registry.register_channel("fake", Arc::new(FakeChannelProvider));

        assert_eq!(
            result,
            Err(PluginError::AlreadyRegistered("fake".to_string()))
        );
    }

    struct ChannelPlugin {
        provider: Arc<FakeChannelProvider>,
    }

    #[async_trait]
    impl Plugin for ChannelPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata {
                id: "channel-plugin".to_string(),
                name: "Channel Plugin".to_string(),
                version: "0.1.0".to_string(),
                kind: PluginKind::Channel,
                description: None,
            }
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Channel]
        }

        async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
            Ok(PluginHandles {
                channels: vec![self.provider.clone()],
                skills: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn init_all_collects_channels_and_updates_status() {
        let mut registry = PluginRegistry::new();
        let plugin = ChannelPlugin {
            provider: Arc::new(FakeChannelProvider),
        };
        registry.load(Box::new(plugin)).unwrap();

        let ctx = PluginContext::default();
        registry.init_all(&ctx).await.unwrap();

        assert!(registry.channel("fake").is_some());
        assert_eq!(
            registry.status().get("channel-plugin"),
            Some(&PluginStatus::Initialized)
        );
    }

    #[tokio::test]
    async fn init_all_marks_failed_plugin_without_panicking() {
        struct FailingPlugin;

        #[async_trait]
        impl Plugin for FailingPlugin {
            fn metadata(&self) -> PluginMetadata {
                PluginMetadata {
                    id: "failing".to_string(),
                    name: "Failing".to_string(),
                    version: "0.1.0".to_string(),
                    kind: PluginKind::Tool,
                    description: None,
                }
            }

            async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
                Err(PluginError::InitFailed(
                    "failing".to_string(),
                    "boom".to_string(),
                ))
            }
        }

        let mut registry = PluginRegistry::new();
        registry.load(Box::new(FailingPlugin)).unwrap();

        let ctx = PluginContext::default();
        registry.init_all(&ctx).await.unwrap();

        assert!(
            matches!(
                registry.status().get("failing"),
                Some(PluginStatus::Failed(_))
            ),
            "expected failed status, got {:?}",
            registry.status().get("failing")
        );
    }

    #[test]
    fn plugin_capabilities_default_to_metadata_kind() {
        let plugin = DummyPlugin {
            id: "tool-only".to_string(),
            kind: PluginKind::Tool,
        };
        assert_eq!(plugin.capabilities(), vec![Capability::Tool]);
    }

    #[test]
    fn load_dir_skips_disabled_and_loads_manifest_plugins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let enabled_dir = plugins_dir.join("enabled-plugin");
        std::fs::create_dir_all(&enabled_dir).unwrap();
        std::fs::write(
            enabled_dir.join("manifest.json"),
            r#"{
                "id": "enabled-plugin",
                "version": "0.1.0",
                "name": "Enabled",
                "capabilities": ["tool"]
            }"#,
        )
        .unwrap();

        let disabled_dir = plugins_dir.join("disabled-plugin");
        std::fs::create_dir_all(&disabled_dir).unwrap();
        std::fs::write(
            disabled_dir.join("manifest.json"),
            r#"{
                "id": "disabled-plugin",
                "version": "0.1.0",
                "name": "Disabled",
                "capabilities": ["channel"]
            }"#,
        )
        .unwrap();

        let mut registry = PluginRegistry::new();
        registry
            .load_dir(&plugins_dir, &["disabled-plugin".to_string()])
            .unwrap();

        assert!(registry.find("enabled-plugin").is_some());
        assert!(registry.find("disabled-plugin").is_none());
        let enabled = registry
            .find("enabled-plugin")
            .expect("enabled plugin should exist");
        assert_eq!(enabled.capabilities(), vec![Capability::Tool]);
    }

    #[test]
    fn topo_sort_orders_by_dependencies() {
        let tmp = tempfile::TempDir::new().unwrap();
        let plugins_dir = tmp.path().join("plugins");

        let manifests = vec![
            (
                plugins_dir.join("b"),
                PluginManifest {
                    id: "b".to_string(),
                    version: "0.1.0".to_string(),
                    name: "B".to_string(),
                    capabilities: vec![Capability::Tool],
                    depends_on: vec!["a".to_string()],
                    ..Default::default()
                },
            ),
            (
                plugins_dir.join("a"),
                PluginManifest {
                    id: "a".to_string(),
                    version: "0.1.0".to_string(),
                    name: "A".to_string(),
                    capabilities: vec![Capability::Tool],
                    depends_on: vec![],
                    ..Default::default()
                },
            ),
            (
                plugins_dir.join("c"),
                PluginManifest {
                    id: "c".to_string(),
                    version: "0.1.0".to_string(),
                    name: "C".to_string(),
                    capabilities: vec![Capability::Tool],
                    depends_on: vec!["b".to_string()],
                    ..Default::default()
                },
            ),
        ];

        let sorted = topo_sort_manifests(&manifests).unwrap();
        let ids: Vec<_> = sorted.into_iter().map(|(_, m)| m.id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn manifest_plugin_rejects_dynamic_library() {
        let manifest = PluginManifest {
            id: "native".to_string(),
            version: "0.1.0".to_string(),
            name: "Native".to_string(),
            capabilities: vec![Capability::Tool],
            library: Some(std::path::PathBuf::from("libnative.so")),
            ..Default::default()
        };
        let plugin = ManifestPlugin::new(manifest, std::path::PathBuf::from("/plugins/native"));

        let result = plugin.init(&PluginContext::default()).await;
        match result {
            Err(PluginError::InitFailed(id, reason)) => {
                assert_eq!(id, "native");
                assert!(reason.contains("dynamic library plugins are not supported"));
            }
            other => panic!("expected InitFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manifest_plugin_fails_on_missing_skill_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = PluginManifest {
            id: "skillful".to_string(),
            version: "0.1.0".to_string(),
            name: "Skillful".to_string(),
            capabilities: vec![Capability::Skill],
            skills: vec![std::path::PathBuf::from("missing/SKILL.md")],
            ..Default::default()
        };
        let plugin = ManifestPlugin::new(manifest, tmp.path().to_path_buf());

        let result = plugin.init(&PluginContext::default()).await;
        match result {
            Err(PluginError::InitFailed(id, reason)) => {
                assert_eq!(id, "skillful");
                assert!(reason.contains("cannot read skill"));
            }
            other => panic!("expected InitFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manifest_plugin_fails_on_invalid_skill_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("SKILL.md"), "no frontmatter here").unwrap();
        let manifest = PluginManifest {
            id: "skillful".to_string(),
            version: "0.1.0".to_string(),
            name: "Skillful".to_string(),
            capabilities: vec![Capability::Skill],
            skills: vec![std::path::PathBuf::from("SKILL.md")],
            ..Default::default()
        };
        let plugin = ManifestPlugin::new(manifest, tmp.path().to_path_buf());

        let result = plugin.init(&PluginContext::default()).await;
        match result {
            Err(PluginError::InitFailed(id, _)) => assert_eq!(id, "skillful"),
            other => panic!("expected InitFailed, got {other:?}"),
        }
    }

    #[test]
    fn load_dir_errors_on_missing_dir() {
        let mut registry = PluginRegistry::new();
        let result = registry.load_dir(std::path::Path::new("/definitely/not/here"), &[]);
        assert!(matches!(result, Err(PluginError::InvalidConfig(_))));
    }

    #[test]
    fn load_dir_errors_on_malformed_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let plugin_dir = tmp.path().join("broken");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("manifest.json"), "{ not json").unwrap();

        let mut registry = PluginRegistry::new();
        let result = registry.load_dir(tmp.path(), &[]);
        assert!(matches!(result, Err(PluginError::InvalidConfig(_))));
    }

    #[test]
    fn load_dir_skips_entries_without_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A subdirectory without manifest.json is skipped...
        std::fs::create_dir_all(tmp.path().join("no-manifest")).unwrap();
        // ...as is a plain file entry.
        std::fs::write(tmp.path().join("not-a-dir.txt"), "ignored").unwrap();

        let ok_dir = tmp.path().join("ok-plugin");
        std::fs::create_dir_all(&ok_dir).unwrap();
        std::fs::write(
            ok_dir.join("manifest.json"),
            r#"{
                "id": "ok-plugin",
                "version": "0.1.0",
                "name": "OK",
                "capabilities": ["tool"]
            }"#,
        )
        .unwrap();

        let mut registry = PluginRegistry::new();
        registry.load_dir(tmp.path(), &[]).unwrap();

        assert_eq!(registry.list().len(), 1);
        assert!(registry.find("ok-plugin").is_some());
    }

    #[tokio::test]
    async fn init_all_marks_duplicate_channel_plugin_failed() {
        struct IdChannelPlugin {
            id: String,
            provider: Arc<FakeChannelProvider>,
        }

        #[async_trait]
        impl Plugin for IdChannelPlugin {
            fn metadata(&self) -> PluginMetadata {
                PluginMetadata {
                    id: self.id.clone(),
                    name: format!("{} plugin", self.id),
                    version: "0.1.0".to_string(),
                    kind: PluginKind::Channel,
                    description: None,
                }
            }

            async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
                Ok(PluginHandles {
                    channels: vec![self.provider.clone()],
                    skills: Vec::new(),
                })
            }
        }

        let mut registry = PluginRegistry::new();
        registry
            .load(Box::new(IdChannelPlugin {
                id: "first".to_string(),
                provider: Arc::new(FakeChannelProvider),
            }))
            .unwrap();
        registry
            .load(Box::new(IdChannelPlugin {
                id: "second".to_string(),
                provider: Arc::new(FakeChannelProvider),
            }))
            .unwrap();

        registry.init_all(&PluginContext::default()).await.unwrap();

        assert_eq!(
            registry.status().get("first"),
            Some(&PluginStatus::Initialized)
        );
        assert_eq!(
            registry.status().get("second"),
            Some(&PluginStatus::Failed(
                "channel 'fake' already registered".to_string()
            ))
        );
    }

    #[test]
    fn topo_sort_detects_missing_dependency() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifests = vec![(
            tmp.path().join("x"),
            PluginManifest {
                id: "x".to_string(),
                version: "0.1.0".to_string(),
                name: "X".to_string(),
                capabilities: vec![Capability::Tool],
                depends_on: vec!["missing".to_string()],
                ..Default::default()
            },
        )];

        assert_eq!(
            topo_sort_manifests(&manifests),
            Err(PluginError::InvalidConfig(
                "plugin 'x' depends on unknown plugin 'missing'".to_string()
            ))
        );
    }

    #[test]
    fn topo_sort_detects_cycle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifests = vec![
            (
                tmp.path().join("x"),
                PluginManifest {
                    id: "x".to_string(),
                    version: "0.1.0".to_string(),
                    name: "X".to_string(),
                    capabilities: vec![Capability::Tool],
                    depends_on: vec!["y".to_string()],
                    ..Default::default()
                },
            ),
            (
                tmp.path().join("y"),
                PluginManifest {
                    id: "y".to_string(),
                    version: "0.1.0".to_string(),
                    name: "Y".to_string(),
                    capabilities: vec![Capability::Tool],
                    depends_on: vec!["x".to_string()],
                    ..Default::default()
                },
            ),
        ];

        assert_eq!(
            topo_sort_manifests(&manifests),
            Err(PluginError::InvalidConfig(
                "circular plugin dependency detected".to_string()
            ))
        );
    }

    #[test]
    fn plugin_kind_skill_round_trip() {
        let json = serde_json::to_string(&PluginKind::Skill).unwrap();
        assert_eq!(json, "\"skill\"");
        let parsed: PluginKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PluginKind::Skill);
    }

    #[test]
    fn plugin_kind_skill_maps_to_capability() {
        assert_eq!(Capability::from(PluginKind::Skill), Capability::Skill);
    }

    #[tokio::test]
    async fn manifest_plugin_loads_skills_from_manifest_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_content =
            "---\nname: plugin-skill\ndescription: A skill from a plugin\n---\nSkill body.";
        std::fs::write(tmp.path().join("SKILL.md"), skill_content).unwrap();

        let manifest = PluginManifest {
            id: "skillful".to_string(),
            version: "0.1.0".to_string(),
            name: "Skillful".to_string(),
            capabilities: vec![Capability::Skill],
            skills: vec![std::path::PathBuf::from("SKILL.md")],
            ..Default::default()
        };

        let plugin = ManifestPlugin::new(manifest, tmp.path().to_path_buf());
        let handles = plugin.init(&PluginContext::default()).await.unwrap();

        assert_eq!(handles.skills.len(), 1);
        assert_eq!(handles.skills[0].frontmatter.name, "plugin-skill");
        assert_eq!(handles.skills[0].source, SkillSource::Plugin);
    }

    struct SkillPlugin {
        skill: Skill,
    }

    #[async_trait]
    impl Plugin for SkillPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata {
                id: "skill-plugin".to_string(),
                name: "Skill Plugin".to_string(),
                version: "0.1.0".to_string(),
                kind: PluginKind::Skill,
                description: None,
            }
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Skill]
        }

        async fn init(&self, _ctx: &PluginContext) -> Result<PluginHandles, PluginError> {
            Ok(PluginHandles {
                channels: Vec::new(),
                skills: vec![self.skill.clone()],
            })
        }
    }

    #[tokio::test]
    async fn init_all_collects_skills_from_plugins() {
        let mut registry = PluginRegistry::new();
        let skill = Skill {
            frontmatter: legion_skills::SkillFrontmatter {
                name: "injected".to_string(),
                description: "Injected skill".to_string(),
                ..default_skill_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Plugin,
            path: std::path::PathBuf::from("/injected/SKILL.md"),
        };
        registry.load(Box::new(SkillPlugin { skill })).unwrap();

        registry.init_all(&PluginContext::default()).await.unwrap();

        assert_eq!(registry.skills().len(), 1);
        assert_eq!(registry.skills()[0].frontmatter.name, "injected");
    }

    fn default_skill_frontmatter() -> legion_skills::SkillFrontmatter {
        legion_skills::SkillFrontmatter {
            name: String::new(),
            description: String::new(),
            when_to_use: None,
            allowed_tools: Vec::new(),
            paths: Vec::new(),
            user_invocable: true,
            model: None,
            effort: None,
        }
    }
}
