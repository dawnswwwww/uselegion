//! Skill listing and validation helpers for the CLI.
//!
//! Skills are loaded fresh on each agent run, so there is no long-lived
//! in-gateway registry to "reload". `legion skills reload` therefore rescans
//! the configured directories and reports any parse errors, which is useful
//! for validating skill files after editing.

use crate::CliError;
use legion_core::config::Config;
use legion_skills::{SkillRegistry, SkillRegistryImpl};

/// List skills configured in `agents.defaults.skills.dirs`.
pub async fn list(config: &Config) -> Result<(), CliError> {
    let skills_config = &config.agents.defaults.skills;
    if !skills_config.enabled {
        println!("Skills are disabled (agents.defaults.skills.enabled = false).");
        return Ok(());
    }
    if skills_config.dirs.is_empty() {
        println!("No skill directories configured (agents.defaults.skills.dirs is empty).");
        return Ok(());
    }

    let mut registry = SkillRegistryImpl::new();
    let _ = registry.load(&skills_config.dirs).await;

    let skills = registry.all();
    if skills.is_empty() {
        println!("No skills found in configured directories.");
        return Ok(());
    }

    println!("{} skill(s) loaded:\n", skills.len());
    for skill in skills {
        println!("  name:        {}", skill.frontmatter.name);
        println!("  description: {}", skill.frontmatter.description);
        println!("  source:      {:?}", skill.source);
        println!("  path:        {}", skill.path.to_string_lossy());
        if !skill.frontmatter.paths.is_empty() {
            println!("  paths:       {}", skill.frontmatter.paths.join(", "));
        }
        if !skill.frontmatter.allowed_tools.is_empty() {
            println!(
                "  tools:       {}",
                skill.frontmatter.allowed_tools.join(", ")
            );
        }
        if let Some(when) = &skill.frontmatter.when_to_use {
            println!("  when:        {}", when);
        }
        println!();
    }
    Ok(())
}

/// Rescan skill directories and report load errors.
pub async fn reload(config: &Config) -> Result<(), CliError> {
    let skills_config = &config.agents.defaults.skills;
    if !skills_config.enabled {
        println!("Skills are disabled (agents.defaults.skills.enabled = false).");
        return Ok(());
    }
    if skills_config.dirs.is_empty() {
        println!("No skill directories configured (agents.defaults.skills.dirs is empty).");
        return Ok(());
    }

    let mut registry = SkillRegistryImpl::new();
    let report = registry.load(&skills_config.dirs).await;

    for name in &report.loaded {
        println!("ok   {}", name);
    }
    for (path, err) in &report.failed {
        println!("err  {}: {}", path.to_string_lossy(), err);
    }

    if report.loaded.is_empty() && report.failed.is_empty() {
        println!("No skills found in configured directories.");
    } else if report.failed.is_empty() {
        println!("\n{} skill(s) loaded successfully.", report.loaded.len());
    } else {
        println!(
            "\n{} loaded, {} failed.",
            report.loaded.len(),
            report.failed.len()
        );
        return Err(CliError::Other(
            "one or more skills failed to load".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_core::config::Config;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_skill(dir: &std::path::Path, name: &str, content: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    fn config_with_skill_dir(dir: PathBuf) -> Config {
        let json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{
                    "defaults": {{
                        "skills": {{
                            "enabled": true,
                            "dirs": ["{}"],
                            "maxSummaryTokens": 100,
                            "maxBodyTokens": 100,
                            "maxTriggeredSkills": 3
                        }}
                    }}
                }}
            }}"#,
            dir.to_string_lossy().replace('\\', "\\\\")
        );
        serde_json::from_str(&json).unwrap()
    }

    #[tokio::test]
    async fn reload_reports_loaded_and_failed_skills() {
        let tmp = TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "rust",
            "---\nname: rust\ndescription: Rust help\n---\nbody",
        );
        write_skill(tmp.path(), "bad", "no frontmatter");

        let config = config_with_skill_dir(tmp.path().to_path_buf());
        let result = reload(&config).await;
        assert!(result.is_err(), "expected failure because of bad skill");
    }

    #[tokio::test]
    async fn reload_succeeds_when_all_skills_valid() {
        let tmp = TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "rust",
            "---\nname: rust\ndescription: Rust help\n---\nbody",
        );

        let config = config_with_skill_dir(tmp.path().to_path_buf());
        reload(&config).await.unwrap();
    }
}
