//! Skill records and the in-memory index.
//!
//! A [`Skill`] is a [`SkillFrontmatter`] (parsed YAML metadata) plus
//! the markdown body that follows it on disk. The [`SkillIndex`] is a
//! flat ordered list plus an optional source directory. When a source
//! directory is known, lookups re-read the directory so runtime edits
//! to `SKILL.md` files take effect without restarting.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

/// Parsed YAML frontmatter at the top of a `SKILL.md` file.
///
/// Mandeven follows Claude Code's spelling for user-facing skill
/// keys such as `allowed-tools` so skills can be ported without
/// pointless schema churn. Only fields used by mandeven are modeled
/// here; unknown frontmatter keys are ignored by serde.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SkillFrontmatter {
    /// Skill identifier; must match the on-disk directory name and
    /// becomes the slash command (`/<name>`).
    pub name: String,
    /// One-sentence description used in the system prompt
    /// `skills_index` and the `/skills` overlay. The model uses this
    /// to decide whether to suggest or invoke the skill.
    pub description: String,
    /// Claude Code-compatible tool allowlist hint. Mandeven treats it
    /// as metadata today; a later permission layer can enforce it.
    #[serde(
        default,
        rename = "allowed-tools",
        deserialize_with = "deserialize_string_list"
    )]
    pub allowed_tools: Vec<String>,
    /// Optional global cron expression. When present, the timer store
    /// materializes a UUID-backed skill timer for this skill.
    #[serde(default)]
    pub timers: Option<String>,
    /// When true, timer-triggered invocations run in a background
    /// session under the fixed cron bucket instead of the active UI
    /// session. Manual `/<name>` invocation remains foreground.
    #[serde(default)]
    pub fork: bool,
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

/// Read-through view of every loaded skill.
///
/// Constructed once by [`crate::skill::load`] at boot and shared via
/// `Arc<SkillIndex>` between the prompt engine, the `SkillTool`, and
/// the CLI's slash-command fallback. When `source_dir` is present,
/// accessors re-read the directory and fall back to the boot
/// snapshot only if the scan itself fails.
#[derive(Clone, Debug, Default)]
pub struct SkillIndex {
    /// Insertion-ordered for stable rendering in the `/skills` overlay
    /// and the system prompt section. Sort order = directory iteration
    /// order at load time, which on most filesystems means
    /// alphabetical.
    skills: Vec<Skill>,
    /// Source directory for hot reload. `None` for test-built indexes
    /// and disabled skill support.
    source_dir: Option<PathBuf>,
}

impl SkillIndex {
    /// Empty index — used when `~/.mandeven/skills/` does not exist
    /// or `[agent.skill] enabled = false`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from a pre-built `Vec<Skill>`. Tests build small
    /// indexes directly; runtime code uses [`Self::from_source`]
    /// when skills are enabled.
    #[must_use]
    pub fn from_skills(skills: Vec<Skill>) -> Self {
        Self {
            skills,
            source_dir: None,
        }
    }

    /// Construct a reloadable index rooted at `source_dir`.
    #[must_use]
    pub fn from_source(skills: Vec<Skill>, source_dir: PathBuf) -> Self {
        Self {
            skills,
            source_dir: Some(source_dir),
        }
    }

    /// Look up a skill by `name`. Linear scan — skill counts are in
    /// the dozens at most.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Skill> {
        self.current_skills()
            .into_iter()
            .find(|s| s.frontmatter.name == name)
    }

    /// `true` when no skills are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.current_skills().is_empty()
    }

    /// Number of loaded skills.
    #[must_use]
    pub fn len(&self) -> usize {
        self.current_skills().len()
    }

    /// Iterate `(name, description)` pairs in load order. The prompt
    /// engine and the `/skills` overlay both consume this view rather
    /// than touching `Skill` directly.
    pub fn entries(&self) -> impl Iterator<Item = (String, String)> {
        self.current_skills()
            .into_iter()
            .map(|s| (s.frontmatter.name, s.frontmatter.description))
    }

    /// Iterate full skill records in load order. Used by runtime
    /// subsystems that consume optional skill metadata such as
    /// timer declarations.
    pub fn skills(&self) -> impl Iterator<Item = Skill> {
        self.current_skills().into_iter()
    }

    fn current_skills(&self) -> Vec<Skill> {
        let Some(source_dir) = &self.source_dir else {
            return self.skills.clone();
        };
        match crate::skill::loader::load_static(source_dir) {
            Ok(skills) => skills,
            Err(err) => {
                eprintln!(
                    "[skill] hot reload failed from {}: {err}; using boot snapshot",
                    source_dir.display()
                );
                self.skills.clone()
            }
        }
    }
}

fn deserialize_string_list<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_yaml::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        serde_yaml::Value::String(s) => Ok(s
            .split_whitespace()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect()),
        serde_yaml::Value::Sequence(items) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                serde_yaml::Value::String(s) => Some(s),
                _ => None,
            })
            .flat_map(|s| {
                s.split_whitespace()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()),
        _ => Ok(Vec::new()),
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
                allowed_tools: Vec::new(),
                timers: None,
                fork: false,
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
        let names: Vec<String> = idx.entries().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["zulu", "alpha"]);
    }

    #[test]
    fn empty_index_is_recognized() {
        let idx = SkillIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }
}
