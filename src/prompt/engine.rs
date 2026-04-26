//! [`PromptEngine`] — single entry point for every prompt the agent
//! constructs.
//!
//! The engine owns:
//!
//! - The boot-time `AGENTS.md` string (read once at [`Self::load`]).
//! - A [`SectionCache`] keyed by section name so every section of
//!   [`Self::iteration_system`] is byte-identical from one call to
//!   the next, keeping `DeepSeek`'s prefix cache hot.
//!
//! Specialized prompts (`title`, `heartbeat_decide`,
//! `compact_summary`) are exposed as thin delegates to the
//! [`super::specialized`] free functions — the engine method is the
//! one stable address the agent imports against, so future per-task
//! caching or per-profile overrides can land without touching every
//! call site.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::llm::Message;

use super::context::{
    AGENTS_MD_NAME, ENV_INFO_NAME, agents_md_section, env_info_section, load_agents_md,
};
use super::error::Result;
use super::section::{Section, SectionCache, SystemPrompt};
use super::specialized;
use super::templates::{INTRO, INTRO_NAME, SYSTEM_RULES, SYSTEM_RULES_NAME, TONE, TONE_NAME};

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
    /// captured once at [`crate::main`] so feeding it through here
    /// avoids re-reading on every iteration.
    pub cwd: &'a Path,
}

/// Owns loaded-once content + the section cache. Constructed once at
/// boot by `main.rs` and shared via `Arc<PromptEngine>` so every
/// call-site (`Agent::iteration`, the heartbeat path, command
/// handlers) sees the same cache state.
pub struct PromptEngine {
    /// `AGENTS.md` body, `None` when the file is absent.
    agents_md: Option<String>,
    cache: SectionCache,
}

impl PromptEngine {
    /// Construct an engine from the per-user data directory
    /// ([`crate::config::home_dir`]).
    ///
    /// Reads `AGENTS.md` once. Subsequent edits to the file are not
    /// picked up until the future `/reload-prompt` command lands.
    ///
    /// # Errors
    ///
    /// - [`super::error::Error::AgentsMdRead`] when `AGENTS.md`
    ///   exists but the read fails.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let agents_md = load_agents_md(data_dir)?;
        Ok(Self {
            agents_md,
            cache: SectionCache::new(),
        })
    }

    /// Build the system prompt for one main-loop iteration.
    ///
    /// Output ordering is the architectural commitment of this
    /// module:
    ///
    /// ```text
    /// intro                  ← static, cached
    /// system_rules           ← static, cached
    /// tone                   ← static, cached
    /// agents_md (optional)   ← dynamic input, cached at first call
    /// env_info               ← dynamic input, cached at first call
    /// ```
    ///
    /// Every section flows through [`SectionCache`] so the rendered
    /// bytes are stable for the lifetime of the engine — the
    /// load-bearing invariant for `DeepSeek`'s automatic prefix
    /// cache. [`Self::clear_cache`] is the one explicit invalidation
    /// path, called from `/compact` after the conversation prefix
    /// has been rewritten.
    #[must_use]
    pub fn iteration_system(&self, ctx: &PromptContext<'_>) -> SystemPrompt {
        let mut prompt = SystemPrompt::new();

        prompt.push(self.cached(INTRO_NAME, || INTRO.to_string()));
        prompt.push(self.cached(SYSTEM_RULES_NAME, || SYSTEM_RULES.to_string()));
        prompt.push(self.cached(TONE_NAME, || TONE.to_string()));

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

    /// Heartbeat phase-1 message envelope. See
    /// [`specialized::heartbeat_decide_messages`].
    #[must_use]
    pub fn heartbeat_decide_messages(&self, content: &str, now: DateTime<Utc>) -> Vec<Message> {
        specialized::heartbeat_decide_messages(content, now)
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

    #[test]
    fn iteration_system_emits_expected_section_order() {
        let dir = tempdir();
        let engine = PromptEngine::load(&dir).unwrap();
        let prompt = engine.iteration_system(&ctx(&dir));

        let names: Vec<&str> = prompt.iter_named().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["intro", "system_rules", "tone", "env_info"]);
    }

    #[test]
    fn iteration_system_includes_agents_md_when_present() {
        let dir = tempdir();
        fs::write(dir.join("AGENTS.md"), "be terse\n").unwrap();
        let engine = PromptEngine::load(&dir).unwrap();
        let prompt = engine.iteration_system(&ctx(&dir));

        let names: Vec<&str> = prompt.iter_named().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec!["intro", "system_rules", "tone", "agents_md", "env_info"]
        );
    }

    #[test]
    fn iteration_system_is_byte_stable_across_calls() {
        // Whole-prompt byte stability is the load-bearing invariant
        // for DeepSeek's automatic prefix cache — every section,
        // not just the static head, must reproduce identically.
        let dir = tempdir();
        let engine = PromptEngine::load(&dir).unwrap();
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
        let engine = PromptEngine::load(&dir).unwrap();
        let before = engine.iteration_system(&ctx(&dir));
        engine.clear_cache();
        let after = engine.iteration_system(&ctx(&dir));

        let b: Vec<_> = before.iter_named().map(|(_, c)| c.to_string()).collect();
        let a: Vec<_> = after.iter_named().map(|(_, c)| c.to_string()).collect();
        assert_eq!(b, a);
    }
}
