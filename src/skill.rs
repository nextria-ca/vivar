//! The vivar skill for coding agents: one SKILL.md (the Agent Skills
//! format Claude Code and Codex both read), shipped in the binary so
//! `vivar skill install` needs no checkout.

use crate::dirs;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const SKILL: &str = include_str!("../skills/vivar/SKILL.md");

/// Where the agents look: user-wide (`~/.claude/skills`, `$CODEX_HOME/skills`)
/// or inside a project (`.claude/skills`, `.agents/skills`).
fn targets(project: Option<&Path>) -> Result<Vec<PathBuf>> {
    Ok(match project {
        Some(root) => vec![
            root.join(".claude").join("skills"),
            root.join(".agents").join("skills"),
        ],
        None => {
            let home = dirs::home()?;
            let codex = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            vec![home.join(".claude").join("skills"), codex.join("skills")]
        }
    })
}

/// Writes the skill into every target directory; returns the files written.
pub fn install(project: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for dir in targets(project)? {
        let dir = dir.join("vivar");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("SKILL.md");
        fs::write(&path, SKILL).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_has_frontmatter_and_lands_in_both_agents_directories() {
        // Line by line: a CRLF checkout must not fail the header check.
        let mut lines = SKILL.lines();
        assert_eq!(lines.next(), Some("---"));
        assert_eq!(lines.next(), Some("name: vivar"));
        assert!(lines.next().is_some_and(|l| l.starts_with("description: ")));
        let tmp = std::env::temp_dir().join(format!("vivar-skill-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let paths = install(Some(&tmp)).unwrap();
        assert_eq!(paths.len(), 2);
        assert!(tmp.join(".claude/skills/vivar/SKILL.md").is_file());
        assert_eq!(
            fs::read_to_string(tmp.join(".agents/skills/vivar/SKILL.md")).unwrap(),
            SKILL
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
