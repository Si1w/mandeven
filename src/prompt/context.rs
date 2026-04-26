//! Boot-time and per-call content that fills the dynamic tail of an
//! assembled [`crate::prompt::SystemPrompt`].
//!
//! Two distinct flavors of "context" — kept in one module because
//! they share the same architectural slot (everything in here is
//! emitted AFTER the static template block):
//!
//! 1. **Boot-time** — `AGENTS.md`, read once by
//!    [`crate::prompt::PromptEngine::load`] and kept in memory for
//!    the lifetime of the engine.
//! 2. **Run-stable** — `env_info` (model id, cwd). Computed each call
//!    from arguments, but the inputs do not change for a given run,
//!    so the rendered bytes go through [`crate::prompt::SectionCache`]
//!    and are byte-stable.
//!
//! Mirrors Claude Code's split between
//! [`getUserContext`](agent-examples/claude-code-analysis/src/context.ts#L155)
//! (CLAUDE.md + currentDate) and
//! [`getSystemContext`](agent-examples/claude-code-analysis/src/context.ts#L116)
//! (git status + cache breaker), simplified to what mandeven needs
//! today. Notably we do **not** carry a current-time field — Claude
//! Code keeps it in a separate `userContext` channel that is also
//! memoized to date-only granularity, but mandeven defers the
//! feature entirely until a concrete need appears.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

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

/// Section name for the catalog of available skills. Sits between
/// the static template block and `AGENTS.md` so the model knows what
/// skills it can suggest before it reads project-specific
/// instructions (which may reference skill names).
pub const SKILLS_INDEX_NAME: &str = "skills_index";

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

/// Wrap `AGENTS.md` content in a [`Section`] under a fixed Markdown
/// header so the model can tell instructions apart from the rest of
/// the prompt.
#[must_use]
pub fn agents_md_section(content: &str) -> Section {
    Section {
        name: AGENTS_MD_NAME,
        content: format!(
            "# Project Instructions (AGENTS.md)\n\n{}",
            content.trim_end()
        ),
    }
}

/// Build the `skills_index` section listing every loaded skill by
/// `name + description`. Returns `None` when no skills are loaded
/// — `iteration_system` then omits the section entirely so the
/// prompt does not carry a misleading "available skills (none)"
/// header.
///
/// The instruction sentence at the top tells the model both
/// invocation paths (`/<name>` for the user, `skill_tool(name)` for
/// the model itself) plus the soft guard "suggest, do not invoke
/// unsolicited" — Claude Code achieves the same with a longer
/// session-guidance bullet, but a one-paragraph framing in front of
/// the list keeps the section under one screen of text.
#[must_use]
pub fn skills_index_section(entries: &[(String, String)]) -> Option<Section> {
    if entries.is_empty() {
        return None;
    }
    let mut content = String::from(
        "# Available Skills\n\
         The user can invoke any of these by typing /<name>. Suggest one \
         proactively when its description matches the current task. You may \
         also call the `skill_tool` to invoke a skill directly when the user \
         has clearly delegated execution.\n",
    );
    for (name, description) in entries {
        writeln!(content, "- /{name}: {description}").expect("writing to String is infallible");
    }
    // Trim the trailing newline so the SystemPrompt::into_message
    // join produces exactly one blank line between sections.
    let content = content.trim_end().to_string();
    Some(Section {
        name: SKILLS_INDEX_NAME,
        content,
    })
}

/// Build the `env_info` section: model id and working directory.
///
/// Mirrors Claude Code's `computeSimpleEnvInfo` (see
/// [`agent-examples/claude-code-analysis/src/constants/prompts.ts:651`])
/// stripped to the two fields that actually matter for a v1 agent.
/// Does not carry a timestamp — see the module docstring.
#[must_use]
pub fn env_info_section(model_id: &str, cwd: &Path) -> Section {
    let content = format!(
        "# Environment\n- Model: {model_id}\n- Working directory: {cwd}",
        cwd = cwd.display(),
    );
    Section {
        name: ENV_INFO_NAME,
        content,
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
    }

    #[test]
    fn skills_index_returns_none_when_empty() {
        let result = skills_index_section(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn skills_index_lists_each_skill_with_slash_prefix() {
        let entries = vec![
            ("git-clean".to_string(), "Clean up branch".to_string()),
            ("rp-generate".to_string(), "Brainstorm research".to_string()),
        ];
        let s = skills_index_section(&entries).unwrap();
        assert!(s.content.starts_with("# Available Skills"));
        assert!(s.content.contains("- /git-clean: Clean up branch"));
        assert!(s.content.contains("- /rp-generate: Brainstorm research"));
        assert!(s.content.contains("skill_tool"));
        assert!(!s.content.ends_with('\n'));
    }

    #[test]
    fn env_info_includes_model_and_cwd_no_timestamp() {
        let s = env_info_section("deepseek-v4-flash", Path::new("/tmp/foo"));
        assert!(s.content.contains("deepseek-v4-flash"));
        assert!(s.content.contains("/tmp/foo"));
        // Must not carry a timestamp — that defeats the prefix cache.
        assert!(!s.content.contains("Current time"));
    }
}
