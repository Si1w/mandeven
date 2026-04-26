//! Skill — markdown-defined extension point that bundles a workflow
//! into a single `SKILL.md` file.
//!
//! Skills are the lowest-friction extension layer mandeven exposes:
//! drop a directory into `~/.mandeven/skills/<name>/SKILL.md`, fill
//! in YAML frontmatter (`name`, `description`), and the body becomes
//! a reusable workflow the user can invoke as `/<name>` or the model
//! can invoke via the `skill_tool`.
//!
//! ## On-disk layout
//!
//! ```text
//! ~/.mandeven/skills/
//!   git-clean/
//!     SKILL.md
//!   rp-generate/
//!     SKILL.md
//!     scripts/        ← optional sibling files; the skill body can
//!                       reference them via path expressions
//! ```
//!
//! ## Two invocation paths
//!
//! 1. **User typed `/<name>`**: the CLI's slash-command parser falls
//!    through to [`SkillIndex::get`], retrieves the body, and emits
//!    it as if the user had typed the body verbatim. This is the
//!    fastest path — one prompt round-trip.
//! 2. **Model called `skill_tool(skill="<name>")`**: the `SkillTool`
//!    looks up the body and injects it as a new user message via the
//!    [`crate::tools::ToolOutcome::Inject`] effect. Same downstream
//!    behavior as path 1 from the model's perspective.
//!
//! Both paths share the same source of truth: a single
//! [`SkillIndex`] loaded once at boot from
//! `<data_dir>/skills/`.
//!
//! ## What v1 deliberately does not do
//!
//! - Embedded shell execution inside skill bodies (Claude Code's
//!   `` !`cmd` `` syntax) — security surface is large and the use
//!   case is rare.
//! - Argument substitution (`${1}`, `${ARG_NAME}`).
//! - Conditional auto-trigger via `paths` glob.
//! - Per-skill tool allowlist or model override.
//! - Bundled (compile-time) skills — mandeven has no "factory
//!   skills" concept.
//! - Plugin / MCP skills — outside scope.
//!
//! Each of these has a clean addition path: extend
//! [`SkillFrontmatter`] with the new field, plumb through
//! [`loader::load`], and react in the [`crate::tools::skill::SkillTool`]
//! call path.

pub mod error;
pub mod loader;
pub mod types;

pub use error::{Error, Result};
pub use loader::{SKILL_FILENAME, load};
pub use types::{Skill, SkillFrontmatter, SkillIndex};

use serde::{Deserialize, Serialize};

/// Subdirectory under [`crate::config::home_dir`] holding skill
/// directories. Same naming convention as `cron/` and `sessions/`.
pub const SKILLS_SUBDIR: &str = "skills";

/// User-tunable knobs for the skill subsystem.
///
/// Intentionally minimal: enable / disable is the only knob today.
/// A future per-skill blocklist (`disabled: [name, ...]`) would land
/// here when the use case appears, but for v1 you can simply rename
/// or move a skill directory to disable it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillConfig {
    /// When `false`, the boot-time scan is skipped entirely:
    /// [`SkillIndex`] is empty, the `skills_index` prompt section is
    /// omitted, and the `SkillTool` refuses every invocation. Default
    /// `true` so dropping a SKILL.md into the directory works without
    /// editing config.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
        }
    }
}

fn default_enabled() -> bool {
    true
}
