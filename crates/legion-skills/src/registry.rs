//! In-memory skill registry with glob-based path matching.

use crate::{LoadReport, Skill, SkillError, SkillSource, parse_skill_md};
use std::collections::HashMap;
use std::path::PathBuf;

/// Storage and query interface for loaded skills.
#[async_trait::async_trait]
pub trait SkillRegistry: Send + Sync {
    /// Scan directories for `SKILL.md` files and parse them.
    async fn load(&mut self, dirs: &[PathBuf]) -> LoadReport;

    /// Add a pre-loaded skill to the registry.
    fn add(&mut self, skill: Skill);

    /// Return skills whose `paths` globs match any of the touched file paths.
    fn match_paths(&self, touched_files: &[String]) -> Vec<&Skill>;

    /// Return skills whose name/description roughly match the intent.
    fn relevant(&self, intent: &str, limit: usize) -> Vec<&Skill>;

    fn get(&self, name: &str) -> Option<&Skill>;
    fn all(&self) -> &[Skill];

    /// Build a prompt block summarizing all loaded skills, truncated to roughly
    /// `max_lines` entries.
    fn summary_block(&self, max_lines: usize) -> String;
}

/// Default in-memory skill registry.
#[derive(Debug, Default)]
pub struct SkillRegistryImpl {
    skills: Vec<Skill>,
    by_name: HashMap<String, usize>,
    path_patterns: Vec<(usize, glob::Pattern)>,
}

impl SkillRegistryImpl {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SkillRegistry for SkillRegistryImpl {
    async fn load(&mut self, dirs: &[PathBuf]) -> LoadReport {
        let mut report = LoadReport::default();
        for dir in dirs {
            let Ok(entries) = tokio::fs::read_dir(dir).await else {
                continue;
            };
            let mut entries = entries;
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let skill_path = path.join("SKILL.md");
                if !skill_path.exists() {
                    continue;
                }
                match tokio::fs::read_to_string(&skill_path).await {
                    Ok(content) => {
                        let path = skill_path.clone();
                        match parse_skill_md(&content, path, SkillSource::Workspace) {
                            Ok(skill) => {
                                let name = skill.frontmatter.name.clone();
                                self.add(skill);
                                report.loaded.push(name);
                            }
                            Err(err) => report.failed.push((skill_path, err)),
                        }
                    }
                    Err(err) => report.failed.push((
                        skill_path.clone(),
                        SkillError::Io {
                            path: skill_path,
                            source: err,
                        },
                    )),
                }
            }
        }
        report
    }

    fn add(&mut self, skill: Skill) {
        let idx = self.skills.len();
        for pattern_str in &skill.frontmatter.paths {
            if let Ok(pattern) = glob::Pattern::new(pattern_str) {
                self.path_patterns.push((idx, pattern));
            } else {
                tracing::warn!(
                    skill = %skill.frontmatter.name,
                    pattern = %pattern_str,
                    "invalid skill path glob"
                );
            }
        }
        self.by_name.insert(skill.frontmatter.name.clone(), idx);
        self.skills.push(skill);
    }

    fn match_paths(&self, touched_files: &[String]) -> Vec<&Skill> {
        let mut matched = Vec::new();
        for file in touched_files {
            for (idx, pattern) in &self.path_patterns {
                if pattern.matches(file) {
                    matched.push(&self.skills[*idx]);
                }
            }
        }
        matched.sort_by_key(|s| &s.frontmatter.name);
        matched.dedup_by(|a, b| a.frontmatter.name == b.frontmatter.name);
        matched
    }

    fn relevant(&self, intent: &str, limit: usize) -> Vec<&Skill> {
        // An empty intent would match every skill via `"".contains("")`,
        // returning unrelated skills; treat it as "no preference".
        if intent.trim().is_empty() {
            return Vec::new();
        }
        let lowered = intent.to_lowercase();
        let mut scored: Vec<(usize, &Skill)> = self
            .skills
            .iter()
            .map(|s| {
                let mut score = 0;
                let name_lower = s.frontmatter.name.to_lowercase();
                let desc_lower = s.frontmatter.description.to_lowercase();
                if name_lower.contains(&lowered) {
                    score += 10;
                }
                if desc_lower.contains(&lowered) {
                    score += 5;
                }
                for word in lowered.split_whitespace() {
                    if name_lower.contains(word) {
                        score += 2;
                    }
                    if desc_lower.contains(word) {
                        score += 1;
                    }
                }
                (score, s)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.0));
        scored.into_iter().map(|(_, s)| s).take(limit).collect()
    }

    fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name).map(|idx| &self.skills[*idx])
    }

    fn all(&self) -> &[Skill] {
        &self.skills
    }

    fn summary_block(&self, max_lines: usize) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut block = String::from("## Skills\n\n");
        for skill in self.skills.iter().take(max_lines) {
            block.push_str(&skill.summary());
            block.push('\n');
        }
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn write_skill(dir: &Path, name: &str, content: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[tokio::test]
    async fn registry_loads_skills_from_directory() {
        let tmp = TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "rust",
            "---\nname: rust\ndescription: Rust help\n---\nbody",
        );
        write_skill(
            tmp.path(),
            "python",
            "---\nname: python\ndescription: Python help\n---\nbody",
        );

        let mut registry = SkillRegistryImpl::new();
        let report = registry.load(&[tmp.path().to_path_buf()]).await;

        assert_eq!(report.loaded.len(), 2);
        assert!(report.failed.is_empty());
        assert!(registry.get("rust").is_some());
        assert!(registry.get("python").is_some());
    }

    #[tokio::test]
    async fn registry_reports_parse_failures() {
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "bad", "no frontmatter");

        let mut registry = SkillRegistryImpl::new();
        let report = registry.load(&[tmp.path().to_path_buf()]).await;

        assert!(report.loaded.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    #[test]
    fn match_paths_finds_skill_by_glob() {
        let mut registry = SkillRegistryImpl::new();
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "terraform".to_string(),
                description: "tf".to_string(),
                paths: vec!["*.tf".to_string()],
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/terraform/SKILL.md"),
        });

        let matched = registry.match_paths(&["main.tf".to_string()]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].frontmatter.name, "terraform");
    }

    #[test]
    fn relevant_matches_description_keywords() {
        let mut registry = SkillRegistryImpl::new();
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "rust".to_string(),
                description: "Help with Rust code".to_string(),
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/rust/SKILL.md"),
        });

        let matched = registry.relevant("rust code", 5);
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn summary_block_lists_skills() {
        let mut registry = SkillRegistryImpl::new();
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "a".to_string(),
                description: "desc".to_string(),
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/a/SKILL.md"),
        });

        let block = registry.summary_block(10);
        assert!(block.contains("## Skills"));
        assert!(block.contains("- a: desc"));
    }

    #[test]
    fn match_paths_dedups_skill_matching_multiple_patterns() {
        let mut registry = SkillRegistryImpl::new();
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "terraform".to_string(),
                description: "tf".to_string(),
                paths: vec!["*.tf".to_string(), "*.tfvars".to_string()],
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/terraform/SKILL.md"),
        });

        let matched = registry.match_paths(&["main.tf".to_string(), "vars.tfvars".to_string()]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].frontmatter.name, "terraform");
    }

    #[test]
    fn load_skips_invalid_glob() {
        let mut registry = SkillRegistryImpl::new();
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "mixed".to_string(),
                description: "m".to_string(),
                paths: vec!["*.tf".to_string(), "[invalid".to_string()],
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/mixed/SKILL.md"),
        });

        // The valid glob still matches; the invalid one is skipped.
        let matched = registry.match_paths(&["main.tf".to_string()]);
        assert_eq!(matched.len(), 1);
        assert!(registry.match_paths(&["[invalid".to_string()]).is_empty());
    }

    #[test]
    fn relevant_empty_intent_returns_nothing() {
        let mut registry = SkillRegistryImpl::new();
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "rust".to_string(),
                description: "Help with Rust code".to_string(),
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/rust/SKILL.md"),
        });

        assert!(registry.relevant("", 5).is_empty());
        assert!(registry.relevant("   ", 5).is_empty());
    }

    #[test]
    fn relevant_respects_limit() {
        let mut registry = SkillRegistryImpl::new();
        for name in ["rust-a", "rust-b", "rust-c"] {
            registry.add(Skill {
                frontmatter: crate::SkillFrontmatter {
                    name: name.to_string(),
                    description: "Help with Rust code".to_string(),
                    ..default_frontmatter()
                },
                body: String::new(),
                source: SkillSource::Workspace,
                path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            });
        }

        let matched = registry.relevant("rust", 2);
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn relevant_name_match_outranks_description() {
        let mut registry = SkillRegistryImpl::new();
        // Skill A: intent hits the description only.
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "helper".to_string(),
                description: "terraform workflows".to_string(),
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/helper/SKILL.md"),
        });
        // Skill B: intent hits the name.
        registry.add(Skill {
            frontmatter: crate::SkillFrontmatter {
                name: "terraform".to_string(),
                description: "infrastructure".to_string(),
                ..default_frontmatter()
            },
            body: String::new(),
            source: SkillSource::Workspace,
            path: PathBuf::from("/skills/terraform/SKILL.md"),
        });

        let matched = registry.relevant("terraform", 5);
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].frontmatter.name, "terraform");
    }

    #[test]
    fn summary_block_empty_registry_is_empty() {
        let registry = SkillRegistryImpl::new();
        assert!(registry.summary_block(10).is_empty());
    }

    fn default_frontmatter() -> crate::SkillFrontmatter {
        crate::SkillFrontmatter {
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
