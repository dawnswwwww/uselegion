use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::expand_tilde;
use crate::memory::{MemoryBackend, MemoryError, MemoryNote, RecallContext};
use crate::prompt::{BuiltPrompt, PromptSection, SectionId, SectionSource, SystemPromptBuilder};
use crate::tools::ToolRegistry;
use crate::types::Reattachment;
use legion_core::config::{Config, StandingOrder};

/// Errors that can occur while assembling the system prompt.
#[derive(Debug, Error)]
pub enum ContextError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("memory error: {0}")]
    Memory(#[from] MemoryError),
}

/// Abstract filesystem used to load bootstrap files. Tests can inject a fake.
#[async_trait]
pub trait Filesystem: Send + Sync {
    async fn read_to_string(&self, path: &Path) -> std::io::Result<String>;
    async fn exists(&self, path: &Path) -> bool;
}

/// Real filesystem implementation based on `tokio::fs`.
pub struct TokioFs;

#[async_trait]
impl Filesystem for TokioFs {
    async fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        tokio::fs::read_to_string(path).await
    }

    async fn exists(&self, path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }
}

/// Resolve the workspace directory for a given agent.
///
/// Per the PRD layout:
/// - The `main` agent defaults to `~/.legion/workspace`.
/// - Other agents default to `<default-workspace>-<agentId>` unless an explicit
///   workspace is configured in `agents.list`.
///
/// `override_`, when `Some`, wins outright (CLI `--workspace` / cwd default in
/// embedded mode). It only affects the "working" layer — tools, bootstrap
/// files, skills, the system prompt — never the memory backend or the gateway.
/// Gateway and channel paths always pass `None`, preserving config authority.
pub fn resolve_workspace(config: &Config, agent_id: &str, override_: Option<&Path>) -> PathBuf {
    if let Some(path) = override_ {
        return expand_tilde(&path.to_string_lossy());
    }

    let explicit = config
        .agents
        .list
        .iter()
        .find(|a| a.id == agent_id)
        .and_then(|a| a.workspace.as_ref())
        .cloned();

    let raw = explicit.unwrap_or_else(|| {
        if agent_id == "main" {
            config.agents.defaults.workspace.clone()
        } else {
            format!("{}-{}", config.agents.defaults.workspace, agent_id)
        }
    });
    expand_tilde(&raw)
}

/// Path to an agent's configuration directory (`~/.legion/agents/<agentId>/agent`).
pub fn agent_dir(agent_id: &str) -> PathBuf {
    expand_tilde(&format!("~/.legion/agents/{agent_id}/agent"))
}

/// Path to an agent's session store directory (`~/.legion/agents/<agentId>/sessions`).
pub fn sessions_dir(agent_id: &str) -> PathBuf {
    expand_tilde(&format!("~/.legion/agents/{agent_id}/sessions"))
}

/// Bootstrap files loaded into the system prompt, in order, with the section
/// each one maps to (prompt-management Phase A). PRD R2's IDENTITY/HEARTBEAT
/// are included; missing files are simply skipped.
const BOOTSTRAP_FILES: &[(&str, SectionId)] = &[
    ("AGENTS.md", SectionId::Agents),
    ("SOUL.md", SectionId::Soul),
    ("USER.md", SectionId::User),
    ("TOOLS.md", SectionId::Tools),
    ("IDENTITY.md", SectionId::Identity),
    ("HEARTBEAT.md", SectionId::Heartbeat),
];

/// Assemble the system prompt from bootstrap files, memory, skills, and per-run
/// overrides.
///
/// When `recalled` is `Some`, those notes are rendered into the
/// `# Relevant memories` section directly (caller-driven recall, e.g. the agent
/// loop's per-turn selector). When `recalled` is `None`, the legacy behavior is
/// preserved: relevant memories are fetched from `memory` only when a
/// `MEMORY.md` bootstrap file is present.
///
/// `agent_prompt` carries the per-agent prompt overrides
/// (`customSystemPrompt` replaces the default `Base` section,
/// `appendSystemPrompt` is appended last, `outputStyle`/`language` become
/// their own sections).
///
/// `standing_orders` is the already-merged list of standing orders (global
/// first, then per-agent); only enabled orders are injected.
#[allow(clippy::too_many_arguments)]
pub async fn assemble_system_prompt(
    workspace: &Path,
    fs: &dyn Filesystem,
    memory: Option<&dyn MemoryBackend>,
    user_message: &str,
    override_prompt: Option<&str>,
    skill_summary_block: Option<&str>,
    skill_body_block: Option<&str>,
    recalled: Option<&[MemoryNote]>,
    agent_prompt: Option<&legion_core::config::AgentConfig>,
    standing_orders: &[StandingOrder],
    todos_enabled: bool,
) -> Result<String, ContextError> {
    Ok(assemble_system_prompt_report(
        workspace,
        fs,
        memory,
        user_message,
        override_prompt,
        skill_summary_block,
        skill_body_block,
        recalled,
        agent_prompt,
        standing_orders,
        todos_enabled,
    )
    .await?
    .text)
}

/// Section-based variant of [`assemble_system_prompt`] that also returns the
/// per-section token report. Sections are priority-resolved
/// (`Override > Coordinator > Agent > Custom > Default`, `Append` last) and
/// concatenated with blank-line separators.
#[allow(clippy::too_many_arguments)]
pub async fn assemble_system_prompt_report(
    workspace: &Path,
    fs: &dyn Filesystem,
    memory: Option<&dyn MemoryBackend>,
    user_message: &str,
    override_prompt: Option<&str>,
    skill_summary_block: Option<&str>,
    skill_body_block: Option<&str>,
    recalled: Option<&[MemoryNote]>,
    agent_prompt: Option<&legion_core::config::AgentConfig>,
    standing_orders: &[StandingOrder],
    todos_enabled: bool,
) -> Result<BuiltPrompt, ContextError> {
    let mut builder = SystemPromptBuilder::new();

    // `customSystemPrompt` registers as the `Base` section with `Custom`
    // source, so it replaces any default Base via source precedence.
    if let Some(cfg) = agent_prompt {
        if let Some(custom) = &cfg.custom_system_prompt {
            builder.add(
                PromptSection::new(SectionId::Base, custom.clone())
                    .with_source(SectionSource::Custom),
            );
        }
    }

    // Standing orders (automation-advanced gap Phase A): persistent
    // authorizations/boundaries from configuration only — never from user
    // messages. Registered near the top so they frame everything that
    // follows; only the instruction text is injected (ids stay internal).
    let enabled_orders: Vec<&StandingOrder> =
        standing_orders.iter().filter(|o| o.enabled).collect();
    if !enabled_orders.is_empty() {
        let lines: Vec<String> = enabled_orders
            .iter()
            .map(|o| format!("- {}", o.instruction))
            .collect();
        builder.add(
            PromptSection::new(
                SectionId::StandingOrders,
                format!("# Standing Orders\n\n{}", lines.join("\n")),
            )
            .with_max_tokens(2000),
        );
        tracing::info!(count = enabled_orders.len(), "standing orders injected");
    }

    for (file, id) in BOOTSTRAP_FILES {
        let path = workspace.join(file);
        if fs.exists(&path).await {
            let content = fs.read_to_string(&path).await?;
            builder.add(PromptSection::new(
                id.clone(),
                format!("# {}\n\n{}", file, content),
            ));
        }
    }

    let memory_path = workspace.join("MEMORY.md");
    let memory_md_exists = fs.exists(&memory_path).await;
    if memory_md_exists {
        let content = fs.read_to_string(&memory_path).await?;
        builder.add(PromptSection::new(
            SectionId::Memory,
            format!("# MEMORY.md\n\n{}", content),
        ));
    }

    let relevant_block = match recalled {
        Some(notes) if !notes.is_empty() => Some(render_relevant_memories(notes)),
        None if memory_md_exists => {
            if let Some(backend) = memory {
                let notes = backend.search(user_message, 5).await?;
                if !notes.is_empty() {
                    Some(render_relevant_memories(&notes))
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(block) = relevant_block {
        // Recalled memory changes every turn: keep it out of any future
        // prompt-cache prefix.
        builder.add(PromptSection::new(SectionId::RelevantMemories, block).uncached());
    }

    if memory_md_exists {
        builder.add(PromptSection::new(
            SectionId::MemoryTools,
            format!(
                "# Memory tools\n\n\
                You have access to `memory_search` (semantic/keyword search), `memory_get` \
                (read a memory file or line range), and `memory_index` (add or update a \
                searchable memory entry). You may also edit `{}` directly with the existing \
                `read`, `write`, and `edit` tools. File edits are reflected immediately in the \
                conversation; use `memory_index` to make a fact searchable in the persistent \
                memory backend without editing the file.",
                memory_path.display()
            ),
        ));
    }

    // Encourage structured decision-making through the `ask_user` tool.
    builder.add(PromptSection::new(
        SectionId::Other("ask_user".to_string()),
        "# Interactive questions\n\n\
        When you need to clarify ambiguity, understand the user's preferences, or make a \
        decision, use the `ask_user` tool to present 2-4 concise options instead of asking \
        open-ended questions in plain text. The user will answer through the same interface \
        they are chatting in."
            .to_string(),
    ));

    if todos_enabled {
        builder.add(PromptSection::new(
            SectionId::TodoInstructions,
            "# Task checklist\n\n\
            For any request that involves multiple steps, use the `todo_write` tool to maintain \
            a short checklist. Call it early with the full plan, then update it as steps start \
            and finish. Keep each item to one line. Use status `in_progress` for the step you are \
            actively working on, `completed` when it is done, and `pending` for steps not yet \
            started. When the plan changes, rewrite the whole list so the user sees the current \
            state."
                .to_string(),
        ));
    }

    if let Some(prompt) = override_prompt {
        builder.add(
            PromptSection::new(
                SectionId::RunOverride,
                format!("# Run override\n\n{}", prompt),
            )
            .with_source(SectionSource::Override),
        );
    }

    if let Some(block) = skill_summary_block {
        if !block.trim().is_empty() {
            builder.add(PromptSection::new(
                SectionId::SkillsSummary,
                block.to_string(),
            ));
        }
    }

    if let Some(block) = skill_body_block {
        if !block.trim().is_empty() {
            builder.add(PromptSection::new(SectionId::SkillsBody, block.to_string()));
        }
    }

    // Per-agent prompt overrides (`agents.list[].outputStyle` / `language` /
    // `appendSystemPrompt`). Style/language register with the agent as their
    // source; `appendSystemPrompt` uses the `Append` source so it always
    // survives precedence resolution and lands at the end of the prompt.
    if let Some(cfg) = agent_prompt {
        if let Some(style) = &cfg.output_style {
            builder.add(
                PromptSection::new(SectionId::OutputStyle, format!("# Output style\n\n{style}"))
                    .with_source(SectionSource::Agent(cfg.id.clone())),
            );
        }
        if let Some(lang) = &cfg.language {
            builder.add(
                PromptSection::new(
                    SectionId::Language,
                    format!("# Language\n\nThe user prefers responses in {lang}."),
                )
                .with_source(SectionSource::Agent(cfg.id.clone())),
            );
        }
        if let Some(append) = &cfg.append_system_prompt {
            builder.add(
                PromptSection::new(SectionId::Append, append.clone())
                    .with_source(SectionSource::Append),
            );
        }
    }

    Ok(builder.build())
}

fn render_relevant_memories(notes: &[MemoryNote]) -> String {
    let rendered: Vec<String> = notes
        .iter()
        .map(|n| format!("- [{}] {}", n.id, n.content))
        .collect();
    format!("# Relevant memories\n\n{}", rendered.join("\n"))
}

/// Shared per-session state used to build compaction reattachments.
#[derive(Clone)]
pub struct SessionContext {
    /// Files the agent has successfully read during this run.
    viewed_files: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
    /// Configured skill names active for this agent.
    active_skills: Vec<String>,
    /// Tool registry used to render the available tool manifest.
    tool_registry: Arc<dyn ToolRegistry>,
    /// Optional memory backend used to recall relevant facts.
    memory_backend: Option<Arc<dyn MemoryBackend>>,
}

impl std::fmt::Debug for SessionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContext")
            .field(
                "viewed_files",
                &self.viewed_files.lock().map(|g| g.len()).unwrap_or(0),
            )
            .field("active_skills", &self.active_skills)
            .field("tool_registry", &"<dyn ToolRegistry>")
            .field("memory_backend", &self.memory_backend.is_some())
            .finish()
    }
}

impl SessionContext {
    /// Create a new session context.
    pub fn new(
        active_skills: Vec<String>,
        tool_registry: Arc<dyn ToolRegistry>,
        memory_backend: Option<Arc<dyn MemoryBackend>>,
    ) -> Self {
        Self {
            viewed_files: Arc::new(std::sync::Mutex::new(HashSet::new())),
            active_skills,
            tool_registry,
            memory_backend,
        }
    }

    /// Register a file path as having been viewed by the agent.
    pub fn mark_viewed_file(&self, path: PathBuf) {
        if let Ok(mut guard) = self.viewed_files.lock() {
            guard.insert(path);
        }
    }

    /// Return a clone of the viewed-files sink for tools to report reads.
    pub fn viewed_files_sink(&self) -> Option<Arc<std::sync::Mutex<HashSet<PathBuf>>>> {
        Some(self.viewed_files.clone())
    }

    /// Return the set of file paths that have been viewed so far.
    pub fn viewed_files(&self) -> HashSet<PathBuf> {
        let guard = self.viewed_files.lock().unwrap_or_else(|e| e.into_inner());
        guard.iter().cloned().collect()
    }

    /// Build reattachments to inject after compaction.
    ///
    /// Reattachments remind the model of its capabilities and the session state
    /// that would otherwise be lost after summarization.
    pub async fn build_reattachments(&self, query: &str) -> Result<Vec<Reattachment>, MemoryError> {
        let mut reattachments = Vec::new();

        let viewed: Vec<String> = {
            let guard = self.viewed_files.lock().unwrap_or_else(|e| e.into_inner());
            guard.iter().map(|p| p.display().to_string()).collect()
        };
        if !viewed.is_empty() {
            reattachments.push(Reattachment::ViewedFiles(viewed));
        }

        if !self.active_skills.is_empty() {
            reattachments.push(Reattachment::ActiveSkills(self.active_skills.clone()));
        }

        let definitions = self.tool_registry.definitions();

        if let Some(backend) = &self.memory_backend {
            let recent_tools: Vec<String> = definitions.iter().map(|d| d.name.clone()).collect();
            let ctx = RecallContext {
                already_surfaced: HashSet::new(),
                recent_tools,
                limit: 5,
            };
            let notes = backend.recall(query, &ctx).await?;
            if !notes.is_empty() {
                reattachments.push(Reattachment::RecalledMemory(
                    notes.into_iter().map(|n| n.content).collect(),
                ));
            }
        }

        if !definitions.is_empty() {
            reattachments.push(Reattachment::ToolManifest(definitions));
        }

        Ok(reattachments)
    }
}

/// In-memory filesystem for tests.
#[derive(Debug, Default, Clone)]
pub struct FakeFs {
    files: HashMap<PathBuf, String>,
}

impl FakeFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(mut self, path: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        self.files.insert(path.into(), content.into());
        self
    }
}

#[async_trait]
impl Filesystem for FakeFs {
    async fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"))
    }

    async fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryBackend, MemoryError, MemoryMeta, MemoryNote};
    use async_trait::async_trait;
    use std::ops::Range;

    struct FakeMemory {
        notes: Vec<MemoryNote>,
    }

    #[async_trait]
    impl MemoryBackend for FakeMemory {
        async fn search(
            &self,
            _query: &str,
            _top_k: usize,
        ) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(self.notes.clone())
        }

        async fn get(
            &self,
            _path: &str,
            _range: Option<Range<usize>>,
        ) -> Result<String, MemoryError> {
            Ok(String::new())
        }

        async fn index(
            &self,
            _id: &str,
            _content: &str,
            _meta: MemoryMeta,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn assembles_prompt_from_bootstrap_files() {
        let fs = FakeFs::new()
            .with_file(PathBuf::from("/ws/AGENTS.md"), "Be helpful.")
            .with_file(PathBuf::from("/ws/SOUL.md"), "You are calm.")
            .with_file(PathBuf::from("/ws/USER.md"), "User likes Rust.");

        let prompt = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        assert!(prompt.contains("Be helpful."));
        assert!(prompt.contains("You are calm."));
        assert!(prompt.contains("User likes Rust."));
        assert!(!prompt.contains("MEMORY.md"));
    }

    #[tokio::test]
    async fn memory_file_triggers_search() {
        let fs = FakeFs::new().with_file(PathBuf::from("/ws/MEMORY.md"), "Root memory.");

        let memory = FakeMemory {
            notes: vec![MemoryNote {
                id: "note-1".into(),
                content: "User prefers dark mode.".into(),
                score: 0.9,
                kind: None,
            }],
        };

        let prompt = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            Some(&memory),
            "theme",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        assert!(prompt.contains("Root memory."));
        assert!(prompt.contains("User prefers dark mode."));
    }

    #[tokio::test]
    async fn recall_drops_notes_matching_tool_names() {
        use crate::tools::{Tool, ToolRegistry};
        use legion_provider::types::ToolDefinition;

        struct FakeRegistry;

        #[async_trait]
        impl ToolRegistry for FakeRegistry {
            fn get(&self, _name: &str) -> Option<Arc<dyn Tool>> {
                None
            }

            fn definitions(&self) -> Vec<ToolDefinition> {
                vec![ToolDefinition {
                    name: "read".to_string(),
                    description: "read a file".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                }]
            }
        }

        let memory = FakeMemory {
            notes: vec![
                MemoryNote {
                    id: "read".into(),
                    content: "read tool doc".into(),
                    score: 0.95,
                    kind: None,
                },
                MemoryNote {
                    id: "fact-1".into(),
                    content: "User prefers dark mode.".into(),
                    score: 0.8,
                    kind: None,
                },
            ],
        };

        let ctx = SessionContext::new(Vec::new(), Arc::new(FakeRegistry), Some(Arc::new(memory)));
        let reattachments = ctx.build_reattachments("theme").await.unwrap();
        let recalled = reattachments
            .iter()
            .find_map(|r| match r {
                Reattachment::RecalledMemory(items) => Some(items),
                _ => None,
            })
            .expect("recalled memory reattachment present");
        assert!(recalled.iter().any(|c| c.contains("dark mode")));
        assert!(!recalled.iter().any(|c| c.contains("read tool doc")));
    }

    #[tokio::test]
    async fn memory_file_adds_tool_instructions() {
        let fs = FakeFs::new().with_file(PathBuf::from("/ws/MEMORY.md"), "Root memory.");

        let prompt = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "theme",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        assert!(prompt.contains("memory_search"));
        assert!(prompt.contains("memory_get"));
        assert!(prompt.contains("memory_index"));
        assert!(prompt.contains("read",));
        assert!(prompt.contains("write"));
        assert!(prompt.contains("edit"));
    }

    #[tokio::test]
    async fn override_prompt_is_appended() {
        let fs = FakeFs::new().with_file(PathBuf::from("/ws/AGENTS.md"), "Be helpful.");

        let prompt = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            Some("Speak like a pirate."),
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        assert!(prompt.contains("Speak like a pirate."));
    }

    #[tokio::test]
    async fn identity_and_heartbeat_bootstrap_files_are_loaded() {
        let fs = FakeFs::new()
            .with_file(PathBuf::from("/ws/AGENTS.md"), "Be helpful.")
            .with_file(PathBuf::from("/ws/IDENTITY.md"), "You are Legion.")
            .with_file(PathBuf::from("/ws/HEARTBEAT.md"), "Check in hourly.");

        let prompt = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        assert!(prompt.contains("# IDENTITY.md\n\nYou are Legion."));
        assert!(prompt.contains("# HEARTBEAT.md\n\nCheck in hourly."));
        // Declaration order: identity before heartbeat, both after agents.
        let agents_pos = prompt.find("# AGENTS.md").unwrap();
        let identity_pos = prompt.find("# IDENTITY.md").unwrap();
        let heartbeat_pos = prompt.find("# HEARTBEAT.md").unwrap();
        assert!(agents_pos < identity_pos && identity_pos < heartbeat_pos);
    }

    #[tokio::test]
    async fn report_exposes_per_section_tokens() {
        let fs = FakeFs::new()
            .with_file(PathBuf::from("/ws/AGENTS.md"), "Be helpful.")
            .with_file(PathBuf::from("/ws/MEMORY.md"), "Root memory.");

        let report = assemble_system_prompt_report(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        let ids: Vec<&SectionId> = report.section_tokens.iter().map(|(id, _)| id).collect();
        assert_eq!(
            ids,
            vec![
                &SectionId::Agents,
                &SectionId::Memory,
                &SectionId::MemoryTools,
                &SectionId::Other("ask_user".to_string())
            ]
        );
        assert!(report.section_tokens.iter().all(|(_, t)| *t > 0));
        assert!(report.truncated.is_empty());
        // The report text matches the legacy string API exactly.
        let legacy = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();
        assert_eq!(report.text, legacy);
    }

    #[tokio::test]
    async fn agent_prompt_overrides_register_sections() {
        let fs = FakeFs::new().with_file(PathBuf::from("/ws/AGENTS.md"), "Be helpful.");
        let cfg = legion_core::config::AgentConfig {
            id: "a1".into(),
            custom_system_prompt: Some("You are a pirate.".into()),
            output_style: Some("Terse bullet points.".into()),
            language: Some("Chinese".into()),
            append_system_prompt: Some("Always sign off.".into()),
            ..Default::default()
        };

        let prompt = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            Some(&cfg),
            &[],
            false,
        )
        .await
        .unwrap();

        // Custom base leads the prompt, before bootstrap files.
        let custom_pos = prompt.find("You are a pirate.").unwrap();
        let agents_pos = prompt.find("# AGENTS.md").unwrap();
        assert!(custom_pos < agents_pos);
        // Style and language sections are injected.
        assert!(prompt.contains("# Output style\n\nTerse bullet points."));
        assert!(prompt.contains("# Language\n\nThe user prefers responses in Chinese."));
        // Append survives at the very end.
        assert!(prompt.ends_with("Always sign off."));
    }

    #[tokio::test]
    async fn custom_base_replaces_default_base_section() {
        let fs = FakeFs::new();
        let cfg = legion_core::config::AgentConfig {
            id: "a1".into(),
            custom_system_prompt: Some("Custom base only.".into()),
            ..Default::default()
        };

        let report = assemble_system_prompt_report(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            Some(&cfg),
            &[],
            false,
        )
        .await
        .unwrap();

        let base_sections: Vec<_> = report
            .section_tokens
            .iter()
            .filter(|(id, _)| *id == SectionId::Base)
            .collect();
        assert_eq!(base_sections.len(), 1);
        assert!(report.text.starts_with("Custom base only."));
    }

    fn standing_order(id: &str, instruction: &str, enabled: bool) -> StandingOrder {
        StandingOrder {
            id: id.to_string(),
            instruction: instruction.to_string(),
            enabled,
        }
    }

    #[tokio::test]
    async fn enabled_standing_orders_are_injected() {
        let fs = FakeFs::new();
        let orders = vec![
            standing_order("g1", "Never touch production databases.", true),
            standing_order("a1", "Always cite sources.", true),
        ];

        let prompt = assemble_system_prompt(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &orders,
            false,
        )
        .await
        .unwrap();

        assert!(prompt.contains(
            "# Standing Orders\n\n- Never touch production databases.\n- Always cite sources."
        ));
        // Order ids are internal and never injected.
        assert!(!prompt.contains("g1"));
        assert!(!prompt.contains("a1"));
    }

    #[tokio::test]
    async fn disabled_standing_orders_are_skipped() {
        let fs = FakeFs::new();
        let orders = vec![
            standing_order("g1", "Never touch production databases.", true),
            standing_order("off", "Do not appear.", false),
        ];

        let report = assemble_system_prompt_report(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &orders,
            false,
        )
        .await
        .unwrap();

        assert!(report.text.contains("Never touch production databases."));
        assert!(!report.text.contains("Do not appear."));
        let ids: Vec<&SectionId> = report.section_tokens.iter().map(|(id, _)| id).collect();
        assert_eq!(
            ids,
            vec![
                &SectionId::StandingOrders,
                &SectionId::Other("ask_user".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn empty_standing_orders_produce_no_section() {
        let fs = FakeFs::new();

        let report = assemble_system_prompt_report(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        assert!(!report.text.contains("Standing Orders"));
        assert!(
            report
                .section_tokens
                .iter()
                .all(|(id, _)| *id != SectionId::StandingOrders)
        );
    }

    #[tokio::test]
    async fn all_disabled_standing_orders_produce_no_section() {
        let fs = FakeFs::new();
        let orders = vec![standing_order("off", "Do not appear.", false)];

        let report = assemble_system_prompt_report(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &orders,
            false,
        )
        .await
        .unwrap();

        assert!(!report.text.contains("Standing Orders"));
        assert!(
            report
                .section_tokens
                .iter()
                .all(|(id, _)| *id != SectionId::StandingOrders),
            "disabled standing orders must not appear"
        );
        assert!(
            report
                .section_tokens
                .iter()
                .any(|(id, _)| *id == SectionId::Other("ask_user".to_string())),
            "ask_user guidance should still be present"
        );
    }

    #[tokio::test]
    async fn todo_instructions_injected_when_enabled() {
        let fs = FakeFs::new();

        let report = assemble_system_prompt_report(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &[],
            true,
        )
        .await
        .unwrap();

        assert!(report.text.contains("todo_write"));
        assert!(
            report
                .section_tokens
                .iter()
                .any(|(id, _)| *id == SectionId::TodoInstructions)
        );
    }

    #[tokio::test]
    async fn todo_instructions_skipped_when_disabled() {
        let fs = FakeFs::new();

        let report = assemble_system_prompt_report(
            Path::new("/ws"),
            &fs,
            None::<&dyn MemoryBackend>,
            "hi",
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .await
        .unwrap();

        assert!(!report.text.contains("todo_write"));
        assert!(
            report
                .section_tokens
                .iter()
                .all(|(id, _)| *id != SectionId::TodoInstructions)
        );
    }

    #[test]
    fn resolves_agent_workspace() {
        let cfg = Config::from_json(
            r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": { "workspace": "~/default" },
                "list": [
                    { "id": "work", "workspace": "/workspaces/work" }
                ]
            }
        }"#,
        )
        .unwrap();

        assert_eq!(
            resolve_workspace(&cfg, "main", None),
            crate::expand_tilde("~/default")
        );
        assert_eq!(
            resolve_workspace(&cfg, "work", None),
            PathBuf::from("/workspaces/work")
        );
        assert_eq!(
            resolve_workspace(&cfg, "other", None),
            crate::expand_tilde("~/default-other")
        );
    }

    #[test]
    fn resolves_agent_workspace_override_wins() {
        let cfg = Config::from_json(
            r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": { "workspace": "~/default" },
                "list": [
                    { "id": "work", "workspace": "/workspaces/work" }
                ]
            }
        }"#,
        )
        .unwrap();

        // Override wins over both config defaults and per-agent config.
        assert_eq!(
            resolve_workspace(&cfg, "main", Some(Path::new("/cwd/here"))),
            PathBuf::from("/cwd/here")
        );
        assert_eq!(
            resolve_workspace(&cfg, "work", Some(Path::new("/cwd/here"))),
            PathBuf::from("/cwd/here")
        );
        // A tilde-style override is expanded like a config path.
        assert_eq!(
            resolve_workspace(&cfg, "main", Some(Path::new("~/projects"))),
            crate::expand_tilde("~/projects")
        );
    }

    #[test]
    fn agent_dir_and_sessions_paths_use_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        assert_eq!(
            agent_dir("main"),
            PathBuf::from(&home).join(".legion/agents/main/agent")
        );
        assert_eq!(
            sessions_dir("work"),
            PathBuf::from(&home).join(".legion/agents/work/sessions")
        );
    }
}
