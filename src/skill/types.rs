//! Skill records and the in-memory index.
//!
//! A [`Skill`] is a [`SkillFrontmatter`] (parsed YAML metadata) plus
//! the markdown body that follows it on disk. The [`SkillIndex`] is a
//! flat ordered list, loaded once at boot, indexed by skill name for
//! `O(n)` lookup — small `n` makes a `HashMap` unnecessary.

use std::path::PathBuf;

use serde::Deserialize;

/// Parsed YAML frontmatter at the top of a `SKILL.md` file.
///
/// v1 holds only the two fields needed for discovery and invocation
/// — additional Claude-Code fields (`when_to_use`, `allowed_tools`,
/// `model`, `paths`, `args`) are deferred until concrete need.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SkillFrontmatter {
    /// Skill identifier; must match the on-disk directory name and
    /// becomes the slash command (`/<name>`).
    pub name: String,
    /// One-sentence description used in the system prompt
    /// `skills_index` and the `/skills` overlay. The model uses this
    /// to decide whether to suggest or invoke the skill.
    pub description: String,
}

/// One loaded skill — frontmatter plus body plus diagnostic source path.
#[derive(Clone, Debug)]
pub struct Skill {
    /// Parsed YAML metadata.
    pub frontmatter: SkillFrontmatter,
    /// Markdown body following the closing `---` of the frontmatter.
    /// Trailing whitespace trimmed; leading whitespace preserved so
    /// embedded code fences keep their indentation.
    pub body: String,
    /// Absolute path to the SKILL.md file. Used in error messages
    /// only — runtime lookups go through [`SkillIndex::get`].
    pub source_path: PathBuf,
}

/// Read-only view of every loaded skill.
///
/// Constructed once by [`crate::skill::load`] at boot and shared via
/// `Arc<SkillIndex>` between the prompt engine, the `SkillTool`, and
/// the CLI's slash-command fallback.
#[derive(Clone, Debug, Default)]
pub struct SkillIndex {
    /// Insertion-ordered for stable rendering in the `/skills` overlay
    /// and the system prompt section. Sort order = directory iteration
    /// order at load time, which on most filesystems means
    /// alphabetical.
    skills: Vec<Skill>,
}

impl SkillIndex {
    /// Empty index — used when `~/.mandeven/skills/` does not exist
    /// or `[agent.skill] enabled = false`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from a pre-built `Vec<Skill>`. The loader uses this
    /// after sorting; tests build small indexes directly.
    #[must_use]
    pub fn from_skills(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    /// Look up a skill by `name`. Linear scan — skill counts are in
    /// the dozens at most.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.frontmatter.name == name)
    }

    /// `true` when no skills are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Number of loaded skills.
    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Iterate `(name, description)` pairs in load order. The prompt
    /// engine and the `/skills` overlay both consume this view rather
    /// than touching `Skill` directly.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.skills.iter().map(|s| {
            (
                s.frontmatter.name.as_str(),
                s.frontmatter.description.as_str(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, desc: &str) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: name.into(),
                description: desc.into(),
            },
            body: String::new(),
            source_path: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
        }
    }

    #[test]
    fn get_returns_skill_by_name() {
        let idx = SkillIndex::from_skills(vec![skill("alpha", "first"), skill("bravo", "second")]);
        assert_eq!(idx.get("alpha").unwrap().frontmatter.description, "first");
        assert_eq!(idx.get("bravo").unwrap().frontmatter.description, "second");
        assert!(idx.get("charlie").is_none());
    }

    #[test]
    fn entries_preserves_insertion_order() {
        let idx = SkillIndex::from_skills(vec![skill("zulu", "z"), skill("alpha", "a")]);
        let names: Vec<&str> = idx.entries().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["zulu", "alpha"]);
    }

    #[test]
    fn empty_index_is_recognized() {
        let idx = SkillIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }
}
