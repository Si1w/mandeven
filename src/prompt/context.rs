//! Boot-time and per-call context that fills the volatile tail of an
//! assembled [`crate::prompt::SystemPrompt`].
//!
//! Two distinct flavors of "context" — kept in one module because
//! they share the same architectural slot (everything in here goes
//! AFTER the cached static block):
//!
//! 1. **Boot-time** — `AGENTS.md`, read once by
//!    [`crate::prompt::PromptEngine::load`] and kept in memory for
//!    the lifetime of the engine. Stable for the run, so it could
//!    arguably be cached, but is marked `cache_break: true` because
//!    the future `/reload-prompt` path will swap the in-memory copy
//!    without invalidating individual sections.
//! 2. **Per-call** — `env_info` (current time, model id, cwd). Always
//!    `cache_break: true` because `now` mutates every call.
//!
//! Mirrors Claude Code's split between
//! [`getUserContext`](agent-examples/claude-code-analysis/src/context.ts#L155)
//! (CLAUDE.md + currentDate) and
//! [`getSystemContext`](agent-examples/claude-code-analysis/src/context.ts#L116)
//! (git status + cache breaker), simplified to what mandeven needs
//! today.

use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::error::{Error, Result};
use super::section::Section;

/// Filename of the per-user agent instructions overlay, resolved
/// relative to [`crate::config::home_dir`]. Mirrors Anthropic's
/// `CLAUDE.md` convention; renamed to the cross-vendor neutral
/// `AGENTS.md` so the file makes sense to readers outside the Claude
/// ecosystem.
pub const AGENTS_FILENAME: &str = "AGENTS.md";

/// Section name for the boot-time `AGENTS.md` overlay.
pub const AGENTS_MD_NAME: &str = "agents_md";

/// Section name for the per-call environment info block.
pub const ENV_INFO_NAME: &str = "env_info";

/// Read `<data_dir>/AGENTS.md` if present.
///
/// Absent file ⇒ `Ok(None)` (the file is optional). Present but
/// unreadable ⇒ [`Error::AgentsMdRead`] — a corrupted permission or
/// disk error here would otherwise produce a silently-degraded
/// prompt every turn, which is worse than a loud boot failure.
///
/// # Errors
///
/// - [`Error::AgentsMdRead`] when the file exists but cannot be
///   read.
pub fn load_agents_md(data_dir: &Path) -> Result<Option<String>> {
    let path = data_dir.join(AGENTS_FILENAME);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::AgentsMdRead { path, source }),
    }
}

/// Wrap `AGENTS.md` content in a [`Section`].
///
/// Uses `cache_break: true` so the future `/reload-prompt` path can
/// swap [`crate::prompt::PromptEngine`]'s in-memory copy without
/// also invalidating the [`crate::prompt::SectionCache`] — see the
/// module docstring for the broader reasoning.
#[must_use]
pub fn agents_md_section(content: &str) -> Section {
    Section {
        name: AGENTS_MD_NAME,
        content: format!(
            "# Project Instructions (AGENTS.md)\n\n{}",
            content.trim_end()
        ),
        cache_break: true,
    }
}

/// Build the `env_info` section: current time, model id, cwd.
///
/// Mirrors Claude Code's `computeSimpleEnvInfo` (see
/// [`agent-examples/claude-code-analysis/src/constants/prompts.ts:651`])
/// stripped to the three fields a v1 agent actually uses. `now` is an
/// argument rather than `Utc::now()` so tests can pin the timestamp.
#[must_use]
pub fn env_info_section(now: DateTime<Utc>, model_id: &str, cwd: &Path) -> Section {
    let content = format!(
        "# Environment\n- Current time: {now}\n- Model: {model_id}\n- Working directory: {cwd}",
        now = now.format("%Y-%m-%d %H:%M:%S UTC"),
        cwd = cwd.display(),
    );
    Section {
        name: ENV_INFO_NAME,
        content,
        cache_break: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let base = env::temp_dir().join(format!("mandeven-prompt-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn load_agents_md_returns_none_when_missing() {
        let dir = tempdir();
        let result = load_agents_md(&dir).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_agents_md_returns_content_when_present() {
        let dir = tempdir();
        fs::write(dir.join(AGENTS_FILENAME), "use snake_case.\n").unwrap();
        let result = load_agents_md(&dir).unwrap();
        assert_eq!(result, Some("use snake_case.\n".to_string()));
    }

    #[test]
    fn agents_md_section_prepends_header_and_trims_trailing_newlines() {
        let s = agents_md_section("project says X\n\n");
        assert!(s.content.starts_with("# Project Instructions"));
        assert!(s.content.ends_with("project says X"));
        assert!(s.cache_break);
    }

    #[test]
    fn env_info_includes_time_model_and_cwd() {
        let now = DateTime::parse_from_rfc3339("2026-04-26T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let s = env_info_section(now, "deepseek-v4-flash", Path::new("/tmp/foo"));
        assert!(s.content.contains("2026-04-26 08:00:00 UTC"));
        assert!(s.content.contains("deepseek-v4-flash"));
        assert!(s.content.contains("/tmp/foo"));
        assert!(s.cache_break);
    }
}
