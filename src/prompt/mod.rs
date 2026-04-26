//! Prompt — assembly engine for every system prompt the agent emits.
//!
//! Layered after Claude Code's `prompts.ts` /
//! `systemPromptSections.ts` / `systemPrompt.ts` / `context.ts` split
//! (see `agent-examples/claude-code-analysis/analysis/04g-prompt-management.md`),
//! pared down to what mandeven needs today:
//!
//! - [`engine::PromptEngine`] — single entry point. Owns the
//!   boot-time `AGENTS.md` body and the [`section::SectionCache`].
//! - [`section::SystemPrompt`] / [`section::Section`] — named, ordered
//!   slices that compose into one [`crate::llm::Message::System`].
//! - [`section::SectionCache`] — `Mutex<HashMap>` memoizer keyed by
//!   section name. Stable sections go through it so the bytes the
//!   model sees are byte-identical turn after turn, keeping
//!   `DeepSeek`'s automatic prefix cache hot.
//! - [`templates`] — the three v1 static sections (`intro`,
//!   `system_rules`, `tone`).
//! - [`context`] — boot-time and per-call dynamic content
//!   (`AGENTS.md`, `env_info`).
//! - [`specialized`] — single-purpose prompts (title generation,
//!   heartbeat phase-1, compact summary) that do **not** share
//!   sections with the main iteration prompt.
//!
//! ## Static / dynamic discipline
//!
//! [`engine::PromptEngine::iteration_system`] always emits sections
//! with `cache_break: false` first and `cache_break: true` last —
//! [`section::SystemPrompt::push`] debug-asserts the rule. Once a
//! volatile section appears, every byte after it is unstable, so
//! placing more stable sections downstream would be a wasted slot.
//!
//! ## Project-local overlay (deferred)
//!
//! `~/.mandeven/AGENTS.md` is the per-user overlay supported in v1. A
//! future per-project `<project>/.agents/AGENTS.md` overlay will stack
//! on top of the global file (Claude Code's `~/.claude/CLAUDE.md` +
//! `<project>/.claude/CLAUDE.md` model). The path constant
//! [`crate::config::PROJECT_OVERRIDE_SUBDIR`] is reserved for that
//! work; the actual stacking logic lives here once it lands.

pub mod context;
pub mod engine;
pub mod error;
pub mod section;
pub mod specialized;
pub mod templates;

pub use context::AGENTS_FILENAME;
pub use engine::{PromptContext, PromptEngine};
pub use error::{Error, Result};
pub use section::{Section, SectionCache, SystemPrompt};
