//! Skill loading and injection for the Legion agent runtime.
//!
//! A skill is a Markdown file with YAML frontmatter that describes a domain
//! capability. The runtime injects a short summary of loaded skills into the
//! system prompt and can inject the full body when a skill is explicitly
//! invoked or matched by file paths.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub mod registry;

pub use registry::{SkillRegistry, SkillRegistryImpl};

/// Effort level hint for a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

/// Where a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Workspace,
    Bundled,
    Plugin,
}

/// Parsed YAML frontmatter of a `SKILL.md` file.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
}

fn default_true() -> bool {
    true
}

/// A loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
    pub source: SkillSource,
    pub path: PathBuf,
}

impl Skill {
    /// One-line summary suitable for the system prompt.
    pub fn summary(&self) -> String {
        format!(
            "- {}: {}",
            self.frontmatter.name, self.frontmatter.description
        )
    }
}

/// Errors that can occur while loading or parsing skills.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid frontmatter in {path}: {source}")]
    InvalidFrontmatter {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    #[error("missing frontmatter in {path}")]
    MissingFrontmatter { path: PathBuf },
    #[error("invalid skill name '{name}' in {path}")]
    InvalidName { name: String, path: PathBuf },
}

/// Parse a `SKILL.md` file into a [`Skill`].
///
/// The file is expected to start with YAML frontmatter delimited by `---`:
///
/// ```markdown
/// ---
/// name: terraform
/// description: Expert help for Terraform files
/// paths: ["*.tf"]
/// ---
///
/// You are a Terraform expert. Prefer `terraform fmt` and state-lock-safe plans.
/// ```
pub fn parse_skill_md(
    content: &str,
    path: impl Into<PathBuf>,
    source: SkillSource,
) -> Result<Skill, SkillError> {
    let path = path.into();
    let (yaml, body) = split_frontmatter(content, &path)?;
    let frontmatter: SkillFrontmatter =
        serde_yaml::from_str(&yaml).map_err(|e| SkillError::InvalidFrontmatter {
            path: path.clone(),
            source: e,
        })?;
    validate_name(&frontmatter.name, &path)?;
    Ok(Skill {
        frontmatter,
        body: body.trim().to_string(),
        source,
        path,
    })
}

fn split_frontmatter(content: &str, path: &Path) -> Result<(String, String), SkillError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(SkillError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    }
    let after_first = &trimmed[3..];
    let Some(end_idx) = after_first.find("\n---") else {
        return Err(SkillError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    };
    let yaml = after_first[..end_idx].trim().to_string();
    let body = after_first[end_idx + 4..].trim_start().to_string();
    Ok((yaml, body))
}

fn validate_name(name: &str, path: &Path) -> Result<(), SkillError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains('\n')
    {
        return Err(SkillError::InvalidName {
            name: name.to_string(),
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Report returned by [`SkillRegistry::load`].
#[derive(Debug, Default)]
pub struct LoadReport {
    pub loaded: Vec<String>,
    pub failed: Vec<(PathBuf, SkillError)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_skill() {
        let content = r#"---
name: terraform
description: Terraform expert
paths:
  - "*.tf"
allowed_tools:
  - read
  - exec
---

You are a Terraform expert.
"#;
        let skill = parse_skill_md(
            content,
            "/skills/terraform/SKILL.md",
            SkillSource::Workspace,
        )
        .unwrap();
        assert_eq!(skill.frontmatter.name, "terraform");
        assert_eq!(skill.frontmatter.description, "Terraform expert");
        assert_eq!(skill.frontmatter.paths, vec!["*.tf"]);
        assert_eq!(skill.frontmatter.allowed_tools, vec!["read", "exec"]);
        assert!(skill.body.contains("Terraform expert"));
    }

    #[test]
    fn parse_rejects_missing_frontmatter() {
        let content = "No frontmatter here.";
        let result = parse_skill_md(content, "/skills/x/SKILL.md", SkillSource::Workspace);
        assert!(matches!(result, Err(SkillError::MissingFrontmatter { .. })));
    }

    #[test]
    fn parse_rejects_invalid_name() {
        let content = r#"---
name: "../evil"
description: bad
---
body
"#;
        let result = parse_skill_md(content, "/skills/x/SKILL.md", SkillSource::Workspace);
        assert!(matches!(result, Err(SkillError::InvalidName { .. })));
    }

    #[test]
    fn parse_rejects_unterminated_frontmatter() {
        let content = "---\nname: x\ndescription: never closed";
        let result = parse_skill_md(content, "/skills/x/SKILL.md", SkillSource::Workspace);
        assert!(matches!(result, Err(SkillError::MissingFrontmatter { .. })));
    }

    #[test]
    fn parse_rejects_invalid_yaml() {
        let content = "---\nname: [unterminated\ndescription: bad\n---\nbody";
        let result = parse_skill_md(content, "/skills/x/SKILL.md", SkillSource::Workspace);
        assert!(matches!(result, Err(SkillError::InvalidFrontmatter { .. })));
    }

    #[test]
    fn parse_defaults_optional_fields() {
        let content = "---\nname: minimal\ndescription: Bare minimum\n---\nbody";
        let skill =
            parse_skill_md(content, "/skills/minimal/SKILL.md", SkillSource::Workspace).unwrap();
        assert_eq!(skill.frontmatter.when_to_use, None);
        assert!(skill.frontmatter.allowed_tools.is_empty());
        assert!(skill.frontmatter.paths.is_empty());
        assert!(skill.frontmatter.user_invocable);
        assert_eq!(skill.frontmatter.model, None);
        assert_eq!(skill.frontmatter.effort, None);
    }

    #[test]
    fn parse_frontmatter_ends_at_first_closing_delimiter() {
        // The closing delimiter search stops at the first `\n---` after the
        // opening delimiter, so `---` lines in the body are kept verbatim.
        let content = "---\nname: x\ndescription: d\n---\nline one\n---\nline two";
        let skill = parse_skill_md(content, "/skills/x/SKILL.md", SkillSource::Workspace).unwrap();
        assert_eq!(skill.frontmatter.name, "x");
        assert_eq!(skill.body, "line one\n---\nline two");
    }

    #[test]
    fn summary_is_one_line() {
        let skill = parse_skill_md(
            "---\nname: rust\ndescription: Rust help\n---\nbody",
            "/skills/rust/SKILL.md",
            SkillSource::Workspace,
        )
        .unwrap();
        assert_eq!(skill.summary(), "- rust: Rust help");
    }
}
