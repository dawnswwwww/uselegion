//! Section-based system prompt assembly (prompt-management gap).
//!
//! The system prompt is modeled as an ordered list of [`PromptSection`]s
//! instead of one opaque string, so each subsystem (bootstrap files, memory,
//! skills, run overrides) registers its own section with an identity, a
//! source, and an optional token cap. [`SystemPromptBuilder::build`] resolves
//! duplicate ids by source precedence (`Override > Coordinator > Agent >
//! Custom > Default` — this is how `customSystemPrompt` replaces the default
//! `Base` section), keeps `Append` sections last, truncates over-cap
//! sections, and reports per-section token usage.

use crate::token_counter::count_tokens;
use serde::{Deserialize, Serialize};

/// Identity of a prompt section. Used for token reporting, truncation
/// reporting, and (from Phase B) priority-based replacement.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionId {
    Base,
    Agents,
    Soul,
    User,
    Tools,
    Identity,
    Heartbeat,
    Memory,
    RelevantMemories,
    MemoryTools,
    SkillsSummary,
    SkillsBody,
    RunOverride,
    EnvInfo,
    Language,
    OutputStyle,
    McpInstructions,
    TodoInstructions,
    Custom,
    Append,
    StandingOrders,
    Other(String),
}

/// Where a section came from. From Phase B this drives the override priority
/// chain (`Override > Coordinator > Agent > Custom > Default`); in Phase A it
/// is recorded for observability only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionSource {
    Default,
    Coordinator,
    Agent(String),
    Custom,
    Override,
    Append,
}

/// A single section of the system prompt.
#[derive(Debug, Clone)]
pub struct PromptSection {
    pub id: SectionId,
    pub content: String,
    pub source: SectionSource,
    /// Whether this section may participate in a provider prompt-cache
    /// prefix. Dynamic sections (env info, recalled memory) should be marked
    /// uncached; [`BuiltPrompt::cache_prefix_len`] reports the longest
    /// leading run of cacheable sections.
    pub cacheable: bool,
    /// Optional token cap; sections exceeding it are truncated line-wise and
    /// reported via [`BuiltPrompt::truncated`].
    pub max_tokens: Option<usize>,
}

impl PromptSection {
    /// A default-sourced, cacheable, uncapped section.
    pub fn new(id: SectionId, content: impl Into<String>) -> Self {
        Self {
            id,
            content: content.into(),
            source: SectionSource::Default,
            cacheable: true,
            max_tokens: None,
        }
    }

    pub fn with_source(mut self, source: SectionSource) -> Self {
        self.source = source;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn uncached(mut self) -> Self {
        self.cacheable = false;
        self
    }
}

/// Result of building the system prompt: the final text plus per-section
/// observability data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltPrompt {
    pub text: String,
    /// Token estimate per emitted section, in registration order.
    pub section_tokens: Vec<(SectionId, usize)>,
    /// Source of each emitted section, parallel to [`Self::section_tokens`].
    pub section_sources: Vec<SectionSource>,
    pub total_tokens: usize,
    /// Sections whose content exceeded their `max_tokens` cap.
    pub truncated: Vec<SectionId>,
    /// Length in bytes of the longest leading run of cacheable sections in
    /// [`Self::text`]. Providers that support prompt caching can treat this
    /// prefix as the stable, cacheable head of the system prompt.
    pub cache_prefix_len: usize,
}

impl BuiltPrompt {
    /// Append a JSONL dump record describing this prompt to
    /// `<dir>/<session_id>.jsonl` (created with mode 0600 on unix). Drives
    /// the `promptDump` config switch, `legion agent --dump-prompts`, and the
    /// `legion context <session>` inspector.
    pub fn write_dump(
        &self,
        dir: &std::path::Path,
        session_id: &str,
    ) -> std::io::Result<std::path::PathBuf> {
        std::fs::create_dir_all(dir)?;
        let file_name = format!("{}.jsonl", session_id.replace(['/', '\\'], "_"));
        let path = dir.join(file_name);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let sections: Vec<serde_json::Value> = self
            .section_tokens
            .iter()
            .enumerate()
            .map(|(i, (id, tokens))| {
                let source = self
                    .section_sources
                    .get(i)
                    .cloned()
                    .unwrap_or(SectionSource::Default);
                serde_json::json!({
                    "id": id,
                    "source": source,
                    "tokens": tokens,
                    "truncated": self.truncated.contains(id),
                })
            })
            .collect();
        let record = serde_json::json!({
            "ts": ts,
            "session": session_id,
            "sections": sections,
            "total_tokens": self.total_tokens,
            "cache_prefix_len": self.cache_prefix_len,
        });
        let mut line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
        line.push('\n');
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(line.as_bytes())?;
        Ok(path)
    }

    /// Split the prompt text into `(content, cache_breakpoint)` system blocks
    /// for providers with prompt caching.
    ///
    /// When `use_prompt_cache` is false or no cacheable prefix exists, returns
    /// a single uncached block. Otherwise the stable leading prefix (up to
    /// [`Self::cache_prefix_len`]) is returned as one cached block, with any
    /// remaining suffix as a second uncached block.
    pub fn split_for_prompt_cache(&self, use_prompt_cache: bool) -> Vec<(String, bool)> {
        if self.text.is_empty() {
            return Vec::new();
        }
        let cache_len = self.cache_prefix_len;
        if !use_prompt_cache || cache_len == 0 {
            return vec![(self.text.clone(), false)];
        }
        if cache_len >= self.text.len() {
            return vec![(self.text.clone(), true)];
        }
        match (self.text.get(..cache_len), self.text.get(cache_len..)) {
            (Some(prefix), Some(suffix)) => {
                vec![(prefix.to_string(), true), (suffix.to_string(), false)]
            }
            // Not a char boundary (should not happen: the length is computed
            // from whole sections) — fall back to a single uncached block.
            _ => vec![(self.text.clone(), false)],
        }
    }
}

/// Ordered collection of prompt sections.
#[derive(Debug, Default)]
pub struct SystemPromptBuilder {
    sections: Vec<PromptSection>,
}

impl SystemPromptBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, section: PromptSection) -> &mut Self {
        self.sections.push(section);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sections.len()
    }

    /// Concatenate sections after priority resolution. Same-id sections keep
    /// only the highest-precedence source (`Override > Coordinator > Agent >
    /// Custom > Default`); `Append`-sourced sections always survive and move
    /// to the end. Distinct ids keep first-registration order. Blank sections
    /// are dropped, over-cap sections are truncated line-wise, and per-section
    /// token usage is reported.
    pub fn build(&self) -> BuiltPrompt {
        let resolved = resolve_sections(&self.sections);
        let mut parts: Vec<String> = Vec::new();
        let mut section_tokens = Vec::new();
        let mut section_sources = Vec::new();
        let mut truncated = Vec::new();
        let mut total_tokens = 0usize;
        // Byte length of `parts` joined so far, and the cacheable prefix.
        let mut text_len = 0usize;
        let mut cache_prefix_len = 0usize;
        let mut prefix_active = true;

        for section in &resolved {
            if section.content.trim().is_empty() {
                continue;
            }
            let (content, was_truncated) = match section.max_tokens {
                Some(cap) if count_tokens(&section.content) > cap => {
                    (truncate_to_token_cap(&section.content, cap), true)
                }
                _ => (section.content.clone(), false),
            };
            let tokens = count_tokens(&content);
            total_tokens += tokens;
            if was_truncated {
                truncated.push(section.id.clone());
            }
            section_tokens.push((section.id.clone(), tokens));
            section_sources.push(section.source.clone());
            // Account for the "\n\n" join separator between emitted sections.
            text_len += if parts.is_empty() { 0 } else { 2 } + content.len();
            if prefix_active && section.cacheable {
                cache_prefix_len = text_len;
            } else {
                prefix_active = false;
            }
            parts.push(content);
        }

        BuiltPrompt {
            text: parts.join("\n\n"),
            section_tokens,
            section_sources,
            total_tokens,
            truncated,
            cache_prefix_len,
        }
    }
}

/// Precedence of a section source (higher wins for duplicate ids).
/// `Append` is handled separately and never competes.
fn source_rank(source: &SectionSource) -> u8 {
    match source {
        SectionSource::Override => 5,
        SectionSource::Coordinator => 4,
        SectionSource::Agent(_) => 3,
        SectionSource::Custom => 2,
        SectionSource::Default => 1,
        SectionSource::Append => 0,
    }
}

/// Resolve sections by source precedence (prompt-management Phase B):
///
/// - For each `SectionId`, only the highest-rank source survives (ties keep
///   the first registration). This is how `customSystemPrompt`
///   (`source = Custom`) replaces a default `Base` section.
/// - `Append`-sourced sections always survive, keep their relative order, and
///   move to the very end (`appendSystemPrompt` semantics).
/// - All other sections keep first-registration order.
pub fn resolve_sections(sections: &[PromptSection]) -> Vec<PromptSection> {
    let mut best: Vec<Option<PromptSection>> = Vec::with_capacity(sections.len());
    let mut append: Vec<PromptSection> = Vec::new();

    for section in sections {
        if section.source == SectionSource::Append {
            append.push(section.clone());
            best.push(None);
            continue;
        }
        let rank = source_rank(&section.source);
        match best.iter_mut().find(|slot| {
            slot.as_ref()
                .is_some_and(|s| s.id == section.id && s.source != SectionSource::Append)
        }) {
            Some(slot) => {
                let current = slot.as_ref().map(|s| source_rank(&s.source)).unwrap_or(0);
                if rank > current {
                    *slot = Some(section.clone());
                }
            }
            None => best.push(Some(section.clone())),
        }
    }

    let mut out: Vec<PromptSection> = best.into_iter().flatten().collect();
    out.extend(append);
    out
}

/// Truncate `content` to at most `cap` estimated tokens, keeping whole lines
/// and appending an explicit truncation marker.
fn truncate_to_token_cap(content: &str, cap: usize) -> String {
    const MARKER: &str = "\n… (section truncated: token cap exceeded)";
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in content.lines() {
        let line_tokens = count_tokens(line);
        if used + line_tokens > cap {
            break;
        }
        used += line_tokens;
        kept.push(line);
    }
    let mut out = kept.join("\n");
    out.push_str(MARKER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_joins_sections_in_registration_order() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(
                SectionId::Agents,
                "# AGENTS.md\n\nBe helpful.",
            ))
            .add(PromptSection::new(SectionId::Soul, "# SOUL.md\n\nBe calm."));
        let built = builder.build();
        assert_eq!(
            built.text,
            "# AGENTS.md\n\nBe helpful.\n\n# SOUL.md\n\nBe calm."
        );
        assert_eq!(built.section_tokens.len(), 2);
        assert_eq!(built.truncated.len(), 0);
        assert!(built.total_tokens > 0);
        assert_eq!(
            built.total_tokens,
            built.section_tokens.iter().map(|(_, t)| t).sum::<usize>()
        );
    }

    #[test]
    fn build_drops_blank_sections() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Agents, "real content"))
            .add(PromptSection::new(SectionId::Soul, "   \n  "));
        let built = builder.build();
        assert_eq!(built.text, "real content");
        assert_eq!(built.section_tokens.len(), 1);
    }

    #[test]
    fn build_truncates_sections_over_token_cap() {
        let long = (0..200)
            .map(|i| format!("line {i} with some words to spend tokens"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut builder = SystemPromptBuilder::new();
        builder.add(PromptSection::new(SectionId::Memory, long).with_max_tokens(20));
        let built = builder.build();
        assert_eq!(built.truncated, vec![SectionId::Memory]);
        assert!(
            built.text.contains("section truncated"),
            "truncation marker expected, got {}",
            built.text
        );
        assert!(
            !built.text.contains("line 199"),
            "tail lines must be dropped, got {}",
            built.text
        );
    }

    #[test]
    fn section_builder_helpers_set_fields() {
        let section = PromptSection::new(SectionId::RunOverride, "x")
            .with_source(SectionSource::Override)
            .with_max_tokens(10)
            .uncached();
        assert_eq!(section.source, SectionSource::Override);
        assert_eq!(section.max_tokens, Some(10));
        assert!(!section.cacheable);
    }

    #[test]
    fn resolve_keeps_highest_rank_source_per_id() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "default base"))
            .add(
                PromptSection::new(SectionId::Base, "custom base")
                    .with_source(SectionSource::Custom),
            )
            .add(
                PromptSection::new(SectionId::Base, "override base")
                    .with_source(SectionSource::Override),
            );
        let built = builder.build();
        assert_eq!(built.text, "override base");
        assert_eq!(built.section_tokens.len(), 1);
    }

    #[test]
    fn resolve_custom_replaces_default_base() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "default base"))
            .add(
                PromptSection::new(SectionId::Base, "custom base")
                    .with_source(SectionSource::Custom),
            )
            .add(PromptSection::new(SectionId::Agents, "agents section"));
        let built = builder.build();
        assert_eq!(built.text, "custom base\n\nagents section");
    }

    #[test]
    fn resolve_agent_source_beats_custom_but_loses_to_coordinator() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(
                PromptSection::new(SectionId::OutputStyle, "custom style")
                    .with_source(SectionSource::Custom),
            )
            .add(
                PromptSection::new(SectionId::OutputStyle, "agent style")
                    .with_source(SectionSource::Agent("a1".into())),
            )
            .add(
                PromptSection::new(SectionId::OutputStyle, "coordinator style")
                    .with_source(SectionSource::Coordinator),
            );
        let built = builder.build();
        assert_eq!(built.text, "coordinator style");
    }

    #[test]
    fn resolve_append_sections_move_to_end_in_order() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(
                PromptSection::new(SectionId::Append, "append one")
                    .with_source(SectionSource::Append),
            )
            .add(PromptSection::new(SectionId::Agents, "middle"))
            .add(
                PromptSection::new(SectionId::Append, "append two")
                    .with_source(SectionSource::Append),
            );
        let built = builder.build();
        assert_eq!(built.text, "middle\n\nappend one\n\nappend two");
    }

    #[test]
    fn resolve_distinct_ids_keep_first_registration_order() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Soul, "soul"))
            .add(PromptSection::new(SectionId::Agents, "agents"))
            .add(
                PromptSection::new(SectionId::Soul, "soul override")
                    .with_source(SectionSource::Override),
            );
        let built = builder.build();
        // Soul keeps its first-registered position even when replaced.
        assert_eq!(built.text, "soul override\n\nagents");
    }

    #[test]
    fn cache_prefix_stops_at_first_uncached_section() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "base"))
            .add(PromptSection::new(SectionId::RelevantMemories, "dynamic").uncached())
            .add(PromptSection::new(SectionId::Agents, "agents"));
        let built = builder.build();
        assert_eq!(built.text, "base\n\ndynamic\n\nagents");
        assert_eq!(built.cache_prefix_len, "base".len());
    }

    #[test]
    fn cache_prefix_covers_all_cacheable_sections() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "base"))
            .add(PromptSection::new(SectionId::Agents, "agents"));
        let built = builder.build();
        assert_eq!(built.cache_prefix_len, built.text.len());
    }

    #[test]
    fn split_for_prompt_cache_marks_prefix_block() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "base"))
            .add(PromptSection::new(SectionId::RelevantMemories, "dynamic").uncached());
        let built = builder.build();

        let blocks = built.split_for_prompt_cache(true);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ("base".to_string(), true));
        assert_eq!(blocks[1], ("\n\ndynamic".to_string(), false));
    }

    #[test]
    fn split_for_prompt_cache_whole_prompt_cacheable() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "base"))
            .add(PromptSection::new(SectionId::Agents, "agents"));
        let built = builder.build();

        let blocks = built.split_for_prompt_cache(true);
        assert_eq!(blocks, vec![(built.text.clone(), true)]);
    }

    #[test]
    fn split_for_prompt_cache_disabled_returns_single_block() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "base"))
            .add(PromptSection::new(SectionId::RelevantMemories, "dynamic").uncached());
        let built = builder.build();

        let blocks = built.split_for_prompt_cache(false);
        assert_eq!(blocks, vec![(built.text.clone(), false)]);
    }

    #[test]
    fn build_reports_section_sources() {
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "base"))
            .add(PromptSection::new(SectionId::Append, "extra").with_source(SectionSource::Append));
        let built = builder.build();
        assert_eq!(
            built.section_sources,
            vec![SectionSource::Default, SectionSource::Append]
        );
        assert_eq!(built.section_sources.len(), built.section_tokens.len());
    }

    #[test]
    fn write_dump_appends_jsonl_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = SystemPromptBuilder::new();
        builder
            .add(PromptSection::new(SectionId::Base, "base"))
            .add(
                PromptSection::new(SectionId::OutputStyle, "style")
                    .with_source(SectionSource::Agent("a1".into())),
            );
        let built = builder.build();
        let path = built.write_dump(dir.path(), "agent:main:default").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let record: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(record["session"], "agent:main:default");
        assert_eq!(record["total_tokens"], built.total_tokens as u64);
        assert_eq!(record["cache_prefix_len"], built.text.len() as u64);
        let sections = record["sections"].as_array().unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0]["id"], "base");
        assert_eq!(sections[0]["source"], "default");
        assert_eq!(sections[1]["id"], "outputStyle");
        assert_eq!(sections[1]["source"], serde_json::json!({"agent": "a1"}));
        assert_eq!(sections[1]["truncated"], false);

        // A second dump appends a second line.
        built.write_dump(dir.path(), "agent:main:default").unwrap();
        let dumped = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = dumped.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
