//! [`PromptEngine`] — single entry point for every prompt the agent
//! constructs.
//!
//! The engine owns:
//!
//! - The boot-time global/project `AGENTS.md` string (read once at
//!   [`PromptEngine::load`]).
//! - A [`SectionCache`] keyed by section name so every section of
//!   [`PromptEngine::iteration_system`] is byte-identical from one call to
//!   the next, keeping `DeepSeek`'s prefix cache hot.
//!
//! Fast-changing context such as `MEMORY.md` is deliberately excluded
//! from this system prompt. The agent injects it as a synthetic user
//! message during request assembly.
//!
//! Specialized prompts (`title`, `compact_summary`) are exposed as
//! thin delegates to the [`super::specialized`] free functions — the
//! engine method is the one stable address the agent imports against,
//! so future per-task caching or per-profile overrides can land
//! without touching every call site.

use std::path::Path;

use crate::llm::Message;
use crate::skill::SkillIndex;

use super::context::{
    AGENTS_MD_NAME, ENV_INFO_NAME, SKILLS_INDEX_NAME, agents_md_section, env_info_section,
    load_agents_md, skills_index_section,
};
use super::error::Result;
use super::section::{Section, SectionCache, SystemPrompt};
use super::specialized;
use super::static_prompt::{STATIC_SYSTEM_SECTIONS, trim_static};

/// Per-call inputs threaded into [`PromptEngine::iteration_system`].
///
/// Kept narrow — every field is required by at least one current
/// section. Adding a field here should be paired with the section
/// that consumes it; otherwise the type drifts into a generic context
/// bag whose callers all populate with `Default::default()`.
#[derive(Debug, Clone, Copy)]
pub struct PromptContext<'a> {
    /// Upstream model identifier (e.g. `"deepseek-v4-flash"`).
    pub model_id: &'a str,
    /// Working directory the agent process was launched from. Already
    /// captured once in `main.rs` so feeding it through here
    /// avoids re-reading on every iteration.
    pub cwd: &'a Path,
}

/// Owns loaded-once content + the section cache. Constructed once at
/// boot by `main.rs` and shared via `Arc<PromptEngine>` so every
/// call-site sees the same cache state.
pub struct PromptEngine {
    /// `AGENTS.md` body, `None` when the file is absent.
    agents_md: Option<String>,
    /// Snapshot of skill `(name, description)` pairs, captured once
    /// at construction so the cached `skills_index` section is built
    /// from a stable input. Empty when no skills are loaded.
    skill_entries: Vec<(String, String)>,
    cache: SectionCache,
}

impl PromptEngine {
    /// Construct an engine from the per-user data directory
    /// ([`crate::config::home_dir`]), launch CWD, and boot-time
    /// [`SkillIndex`].
    ///
    /// Reads global + project `AGENTS.md` files once and snapshots
    /// the skill catalog into a `Vec<(name, description)>` — the
    /// engine doesn't keep an `Arc<SkillIndex>` because the only
    /// piece of skill state it needs is the index for the prompt
    /// section. The rest of the agent reaches the index through its
    /// own `Arc`.
    ///
    /// # Errors
    ///
    /// - [`super::error::Error::AgentsMdRead`] when `AGENTS.md`
    ///   exists but the read fails.
    pub fn load(data_dir: &Path, cwd: &Path, skills: &SkillIndex) -> Result<Self> {
        let agents_md = load_agents_md(data_dir, cwd)?;
        let skill_entries: Vec<(String, String)> = skills
            .entries()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect();
        Ok(Self {
            agents_md,
            skill_entries,
            cache: SectionCache::new(),
        })
    }

    /// Build the system prompt for one main-loop iteration.
    ///
    /// Output ordering is the architectural commitment of this
    /// module:
    ///
    /// ```text
    /// intro                     ← static, cached
    /// system_rules              ← static, cached
    /// doing_tasks               ← static, cached
    /// actions                   ← static, cached
    /// using_tools               ← static, cached
    /// tone                      ← static, cached
    /// skills_index (optional)   ← cached, omitted when no skills
    /// agents_md (optional)      ← cached
    /// env_info                  ← cached
    /// ```
    ///
    /// `skills_index` lands ahead of `agents_md` because AGENTS.md
    /// may reference skill names ("for git use /git-clean"); the
    /// model needs to know what skills exist before reading the
    /// project-specific instructions.
    ///
    /// Every section flows through [`SectionCache`] so the rendered
    /// bytes are stable for the lifetime of the engine — the
    /// load-bearing invariant for `DeepSeek`'s automatic prefix
    /// cache. [`Self::clear_cache`] is the one explicit invalidation
    /// path, called from `/compact` after the conversation prefix
    /// has been rewritten or from `/switch` when model metadata changes.
    ///
    /// # Panics
    ///
    /// Panics if the section cache mutex was poisoned by a prior
    /// compute call — irrecoverable.
    #[must_use]
    pub fn iteration_system(&self, ctx: &PromptContext<'_>) -> SystemPrompt {
        let mut prompt = SystemPrompt::new();

        for section in STATIC_SYSTEM_SECTIONS {
            prompt.push(self.cached(section.name, || trim_static(section.content)));
        }

        if !self.skill_entries.is_empty() {
            let entries = self.skill_entries.clone();
            prompt.push(self.cached(SKILLS_INDEX_NAME, move || {
                // `skills_index_section` returns Some when entries
                // is non-empty; we already gated on that above.
                skills_index_section(&entries)
                    .expect("non-empty entries produce Some")
                    .content
            }));
        }

        if let Some(content) = &self.agents_md {
            let body = content.clone();
            prompt.push(self.cached(AGENTS_MD_NAME, move || agents_md_section(&body).content));
        }
        let model_id = ctx.model_id.to_string();
        let cwd = ctx.cwd.to_path_buf();
        prompt.push(self.cached(ENV_INFO_NAME, move || {
            env_info_section(&model_id, &cwd).content
        }));

        prompt
    }

    /// Title-generation message envelope. See [`specialized::title_messages`].
    #[must_use]
    pub fn title_messages(&self, user_input: &str) -> Vec<Message> {
        specialized::title_messages(user_input)
    }

    /// Compact-summary system text, optionally extended with a focus
    /// area. See [`specialized::compact_summary_system`].
    #[must_use]
    pub fn compact_summary_system(&self, focus: Option<&str>) -> String {
        specialized::compact_summary_system(focus)
    }

    /// Drop every cached section. Wired up to `/compact` so a
    /// post-compaction run rebuilds its sections — same timing as
    /// Claude Code's `clearSystemPromptSections()`.
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Build a section by funneling its content through the cache.
    /// First call computes and stores; subsequent calls return the
    /// stored clone.
    fn cached<F>(&self, name: &'static str, compute: F) -> Section
    where
        F: FnOnce() -> String,
    {
        Section {
            name,
            content: self.cache.get_or_compute(name, compute),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, SkillFrontmatter};
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let base = env::temp_dir().join(format!("mandeven-prompt-engine-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn ctx(cwd: &Path) -> PromptContext<'_> {
        PromptContext {
            model_id: "deepseek-v4-flash",
            cwd,
        }
    }

    fn engine_no_skills(data_dir: &Path) -> PromptEngine {
        PromptEngine::load(data_dir, data_dir, &SkillIndex::new()).unwrap()
    }

    fn engine_with_skills(data_dir: &Path, skills: Vec<(&str, &str)>) -> PromptEngine {
        let skills: Vec<Skill> = skills
            .into_iter()
            .map(|(n, d)| Skill {
                frontmatter: SkillFrontmatter {
                    name: n.into(),
                    description: d.into(),
                    allowed_tools: Vec::new(),
                    timers: None,
                    fork: false,
                },
                body: String::new(),
                source_path: PathBuf::from(format!("/tmp/{n}/SKILL.md")),
            })
            .collect();
        PromptEngine::load(data_dir, data_dir, &SkillIndex::from_skills(skills)).unwrap()
    }

    #[test]
    fn iteration_system_emits_expected_section_order_without_skills() {
        let dir = tempdir();
        let engine = engine_no_skills(&dir);
        let prompt = engine.iteration_system(&ctx(&dir));

        let names: Vec<&str> = prompt.iter_named().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec![
                "intro",
                "system_rules",
                "doing_tasks",
                "actions",
                "using_tools",
                "tone",
                "env_info",
            ]
        );
    }

    #[test]
    fn iteration_system_inserts_skills_index_before_agents_md() {
        let dir = tempdir();
        fs::write(dir.join("AGENTS.md"), "be terse\n").unwrap();
        let engine = engine_with_skills(&dir, vec![("git-clean", "Clean up branch")]);
        let prompt = engine.iteration_system(&ctx(&dir));

        let names: Vec<&str> = prompt.iter_named().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec![
                "intro",
                "system_rules",
                "doing_tasks",
                "actions",
                "using_tools",
                "tone",
                "skills_index",
                "agents_md",
                "env_info",
            ]
        );
    }

    #[test]
    fn iteration_system_omits_skills_index_when_no_skills_loaded() {
        let dir = tempdir();
        fs::write(dir.join("AGENTS.md"), "be terse\n").unwrap();
        let engine = engine_no_skills(&dir);
        let prompt = engine.iteration_system(&ctx(&dir));
        let names: Vec<&str> = prompt.iter_named().map(|(n, _)| n).collect();
        assert!(!names.contains(&"skills_index"));
    }

    #[test]
    fn iteration_system_is_byte_stable_across_calls() {
        // Whole-prompt byte stability is the load-bearing invariant
        // for DeepSeek's automatic prefix cache — every section,
        // not just the static head, must reproduce identically.
        let dir = tempdir();
        let engine = engine_no_skills(&dir);
        let p1 = engine.iteration_system(&ctx(&dir));
        let p2 = engine.iteration_system(&ctx(&dir));

        let s1: Vec<_> = p1.iter_named().collect();
        let s2: Vec<_> = p2.iter_named().collect();
        assert_eq!(s1, s2);
    }

    #[test]
    fn clear_cache_does_not_change_section_content() {
        // Cache eviction must produce the same bytes on the next
        // build — only the cache hit/miss path changes. This is the
        // load-bearing invariant for `/compact` followed by an
        // immediate iteration.
        let dir = tempdir();
        let engine = engine_no_skills(&dir);
        let before = engine.iteration_system(&ctx(&dir));
        engine.clear_cache();
        let after = engine.iteration_system(&ctx(&dir));

        let b: Vec<_> = before.iter_named().map(|(_, c)| c.to_string()).collect();
        let a: Vec<_> = after.iter_named().map(|(_, c)| c.to_string()).collect();
        assert_eq!(b, a);
    }
}
