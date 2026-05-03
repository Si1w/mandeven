//! Boot-time and per-call content that fills the dynamic tail of an
//! assembled [`crate::prompt::SystemPrompt`].
//!
//! Three distinct flavors of "context" — kept in one module because
//! they share the same architectural slot (everything in here is
//! emitted AFTER the static template block):
//!
//! 1. **Boot-time** — global and project-local `AGENTS.md` files,
//!    read once by [`crate::prompt::PromptEngine::load`] and kept in
//!    memory for the lifetime of the engine.
//! 2. **Turn snapshot** — `skills_index`, rebuilt from a
//!    [`crate::skill::SkillSnapshot`] captured before request
//!    assembly so runtime skill edits appear on the next turn without
//!    changing a turn mid-flight.
//! 3. **Run-stable** — `env_info` (model id, cwd). Computed each call
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
use std::path::{Path, PathBuf};

use super::error::{Error, Result};
use super::section::Section;

/// Filename of agent instruction files. Mandeven reads both the
/// per-user file under [`crate::config::home_dir`] and any matching
/// files discovered from the launch CWD upward, mirroring Codex's
/// repository-level convention.
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

/// One loaded `AGENTS.md` source.
#[derive(Debug)]
struct AgentsMdSource {
    scope: &'static str,
    path: PathBuf,
    content: String,
}

/// Read global `<data_dir>/AGENTS.md` plus project-local `AGENTS.md`
/// files from `cwd` and its ancestors.
///
/// Absent files ⇒ `Ok(None)` (instructions are optional). Present
/// but unreadable files ⇒ [`Error::AgentsMdRead`] — a corrupted
/// permission or disk error here would otherwise produce a
/// silently-degraded prompt every turn, which is worse than a loud
/// boot failure.
///
/// # Errors
///
/// - [`Error::AgentsMdRead`] when the file exists but cannot be
///   read.
pub fn load_agents_md(data_dir: &Path, cwd: &Path) -> Result<Option<String>> {
    let global_path = data_dir.join(AGENTS_FILENAME);
    let mut sources = Vec::new();
    if let Some(content) = read_optional_agents_md(&global_path)? {
        sources.push(AgentsMdSource {
            scope: "Global Instructions",
            path: global_path.clone(),
            content,
        });
    }

    for path in project_agents_paths(cwd) {
        if same_path(&path, &global_path) {
            continue;
        }
        if let Some(content) = read_optional_agents_md(&path)? {
            sources.push(AgentsMdSource {
                scope: "Project Instructions",
                path,
                content,
            });
        }
    }

    if sources.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format_agents_md_sources(&sources)))
    }
}

fn read_optional_agents_md(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::AgentsMdRead {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn project_agents_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = cwd
        .ancestors()
        .map(|ancestor| ancestor.join(AGENTS_FILENAME))
        .collect();
    paths.reverse();
    paths
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn format_agents_md_sources(sources: &[AgentsMdSource]) -> String {
    let mut out = String::new();
    for (idx, source) in sources.iter().enumerate() {
        if idx > 0 {
            out.push_str("\n\n");
        }
        writeln!(out, "## {}", source.scope).expect("writing to String is infallible");
        writeln!(out, "Path: {}", source.path.display()).expect("writing to String is infallible");
        out.push('\n');
        out.push_str(source.content.trim_end());
    }
    out
}

/// Wrap `AGENTS.md` content in a [`Section`] under a fixed Markdown
/// header so the model can tell instructions apart from the rest of
/// the prompt.
#[must_use]
pub fn agents_md_section(content: &str) -> Section {
    Section {
        name: AGENTS_MD_NAME,
        content: format!("# Agent Instructions (AGENTS.md)\n\n{}", content.trim_end()),
    }
}

/// Build the `skills_index` section listing every loaded skill by
/// `name + description`. Returns `None` when no skills are loaded
/// — `iteration_system` then omits the section entirely so the
/// prompt does not carry a misleading "available skills (none)"
/// header.
///
/// The instruction sentence at the top tells the model both
/// invocation paths (`/<name>` for the user, `skill_use(name)` for
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
         also call `skill_use` to invoke a skill directly when the user \
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
/// `agent-examples/claude-code-analysis/src/constants/prompts.ts:651`)
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
        let result = load_agents_md(&dir, &dir).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_agents_md_returns_global_content_when_present() {
        let dir = tempdir();
        fs::write(dir.join(AGENTS_FILENAME), "use snake_case.\n").unwrap();
        let result = load_agents_md(&dir, &dir).unwrap().unwrap();
        assert!(result.contains("## Global Instructions"));
        assert!(result.contains("use snake_case."));
    }

    #[test]
    fn load_agents_md_stacks_global_then_project_files() {
        let data = tempdir();
        let project = tempdir().join("repo").join("nested");
        fs::create_dir_all(&project).unwrap();
        fs::write(data.join(AGENTS_FILENAME), "global rule\n").unwrap();
        fs::write(
            project.parent().unwrap().join(AGENTS_FILENAME),
            "repo rule\n",
        )
        .unwrap();
        fs::write(project.join(AGENTS_FILENAME), "nested rule\n").unwrap();

        let result = load_agents_md(&data, &project).unwrap().unwrap();
        let global = result.find("global rule").unwrap();
        let repo = result.find("repo rule").unwrap();
        let nested = result.find("nested rule").unwrap();
        assert!(global < repo);
        assert!(repo < nested);
    }

    #[test]
    fn agents_md_section_prepends_header_and_trims_trailing_newlines() {
        let s = agents_md_section("project says X\n\n");
        assert!(s.content.starts_with("# Agent Instructions"));
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
        assert!(s.content.contains("skill_use"));
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
