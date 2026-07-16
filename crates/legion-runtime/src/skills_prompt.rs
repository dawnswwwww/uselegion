//! Skill body prompt injection for the agent runtime.
//!
//! This module renders full skill bodies into a system-prompt section and
//! truncates the result to a token budget. It lives in `legion-runtime` so it
//! can reuse the existing `token_counter`.

use legion_skills::Skill;

/// Render the full bodies of `skills` into a prompt block and truncate to
/// `max_tokens`.
///
/// The block contains each skill's name, body, and declared allowed tools. If
/// the total token count exceeds `max_tokens`, skills are dropped from the end
/// until the remaining block fits. If a single skill's body already exceeds the
/// budget, its body is truncated and a "(truncated)" marker is appended.
pub fn skill_body_block(skills: &[&Skill], max_tokens: usize) -> String {
    if skills.is_empty() || max_tokens == 0 {
        return String::new();
    }

    let mut block = String::from("## Skill bodies\n\n");

    for skill in skills {
        let skill_text = render_skill(skill);
        let candidate = format!("{}{}\n", block, skill_text);
        let candidate_tokens = crate::token_counter::count_tokens(&candidate);

        if candidate_tokens <= max_tokens {
            block = candidate;
            continue;
        }

        // This skill doesn't fit whole. Try truncating just this skill and
        // appending it to the skills already collected; otherwise stop.
        let used_tokens = crate::token_counter::count_tokens(&block);
        let remaining = max_tokens.saturating_sub(used_tokens);
        if remaining == 0 {
            break;
        }
        let truncated = truncate_skill_body(&skill_text, remaining);
        let candidate_with_truncated = format!("{}{}\n", block, truncated);
        if crate::token_counter::count_tokens(&candidate_with_truncated) <= max_tokens {
            block = candidate_with_truncated;
        }
        break;
    }

    block
}

fn render_skill(skill: &Skill) -> String {
    let mut text = format!("### Skill: {}\n\n{}", skill.frontmatter.name, skill.body);
    if !skill.frontmatter.allowed_tools.is_empty() {
        text.push_str("\n\nAllowed tools: ");
        text.push_str(&skill.frontmatter.allowed_tools.join(", "));
    }
    text
}

/// Truncate a single rendered skill so that its token count fits in
/// `max_tokens`. The marker is appended after truncation, so the result may be
/// slightly under the budget.
fn truncate_skill_body(rendered: &str, max_tokens: usize) -> String {
    if crate::token_counter::count_tokens(rendered) <= max_tokens {
        return rendered.to_string();
    }

    // Binary search for the longest prefix (by char boundary) that fits.
    let mut low = 0usize;
    let mut high = rendered.len();
    while low < high {
        let mid = (low + high).div_ceil(2);
        if !rendered.is_char_boundary(mid) {
            // Move to the next char boundary to keep the loop safe.
            let mut next = mid;
            while next < rendered.len() && !rendered.is_char_boundary(next) {
                next += 1;
            }
            if next == mid {
                high = mid - 1;
                continue;
            }
            let candidate = format!("{}... (truncated)", &rendered[..next]);
            if crate::token_counter::count_tokens(&candidate) <= max_tokens {
                low = next;
            } else {
                high = mid - 1;
            }
            continue;
        }

        let candidate = format!("{}... (truncated)", &rendered[..mid]);
        if crate::token_counter::count_tokens(&candidate) <= max_tokens {
            low = mid;
        } else {
            high = mid - 1;
        }
    }

    while low > 0 && !rendered.is_char_boundary(low) {
        low -= 1;
    }
    format!("{}... (truncated)", &rendered[..low])
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_skills::{Skill, SkillFrontmatter, SkillSource};
    use std::path::PathBuf;

    fn make_skill(name: &str, body: &str, allowed_tools: &[&str], paths: &[&str]) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: name.to_string(),
                description: format!("{name} help"),
                when_to_use: None,
                allowed_tools: allowed_tools.iter().map(|s| s.to_string()).collect(),
                paths: paths.iter().map(|s| s.to_string()).collect(),
                user_invocable: true,
                model: None,
                effort: None,
            },
            body: body.to_string(),
            source: SkillSource::Workspace,
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        }
    }

    #[test]
    fn empty_skills_returns_empty() {
        assert!(skill_body_block(&[], 100).is_empty());
    }

    #[test]
    fn renders_single_skill() {
        let skill = make_skill("rust", "You are a Rust expert.", &["read", "exec"], &[]);
        let block = skill_body_block(&[&skill], 10_000);
        assert!(block.contains("## Skill bodies"));
        assert!(block.contains("### Skill: rust"));
        assert!(block.contains("You are a Rust expert."));
        assert!(block.contains("Allowed tools: read, exec"));
    }

    #[test]
    fn renders_multiple_skills_in_order() {
        let a = make_skill("a", "body a", &[], &[]);
        let b = make_skill("b", "body b", &[], &[]);
        let block = skill_body_block(&[&a, &b], 10_000);
        let pos_a = block.find("body a").unwrap();
        let pos_b = block.find("body b").unwrap();
        assert!(pos_a < pos_b);
    }

    #[test]
    fn truncation_drops_skills_to_fit_budget() {
        let a = make_skill("a", "some body text", &[], &[]);
        let b = make_skill("b", "more body text here", &[], &[]);
        // Budget is large enough for header + first skill but not both.
        let block_a = skill_body_block(&[&a], usize::MAX);
        let budget = crate::token_counter::count_tokens(&block_a);
        let block_ab = skill_body_block(&[&a, &b], budget);
        assert!(block_ab.contains("### Skill: a"));
        assert!(!block_ab.contains("### Skill: b"));
    }

    #[test]
    fn truncation_keeps_skill_boundaries() {
        let a = make_skill("a", "first skill body", &[], &[]);
        let b = make_skill("b", "second skill body", &[], &[]);
        // Budget should fit exactly the header + skill a; skill b must be
        // omitted rather than torn.
        let block_a = skill_body_block(&[&a], usize::MAX);
        let block_ab = skill_body_block(&[&a, &b], crate::token_counter::count_tokens(&block_a));
        assert_eq!(block_a, block_ab);
    }

    #[test]
    fn single_skill_truncation_adds_marker() {
        let skill = make_skill("long", &"a".repeat(10_000), &[], &[]);
        let block = skill_body_block(&[&skill], 50);
        assert!(block.contains("### Skill: long"));
        assert!(block.contains("... (truncated)"));
        assert!(!block.contains(&"a".repeat(10_000)));
    }

    #[test]
    fn zero_budget_returns_empty() {
        let skill = make_skill("x", "body", &[], &[]);
        assert!(skill_body_block(&[&skill], 0).is_empty());
    }

    #[test]
    fn truncation_handles_multibyte_characters() {
        // Full-width parentheses are 3-byte UTF-8 characters; the binary
        // search must not slice in the middle of a character.
        let body = "（".repeat(100);
        let skill = make_skill("wide", &body, &[], &[]);
        let block = skill_body_block(&[&skill], 50);
        assert!(block.contains("### Skill: wide"));
        assert!(block.contains("... (truncated)"));
        // No panic and the result is valid UTF-8.
        assert!(block.chars().next().is_some());
    }
}
