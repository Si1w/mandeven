//! Prompt — assembly engine for every system prompt the agent emits.
//!
//! Layered after Claude Code's `prompts.ts` /
//! `systemPromptSections.ts` / `systemPrompt.ts` / `context.ts` split
//! (see `agent-examples/claude-code-analysis/analysis/04g-prompt-management.md`),
//! pared down to what mandeven needs today:
//!
//! - [`engine::PromptEngine`] — single entry point. Owns the
//!   boot-time `AGENTS.md` body, skill index handle, and the
//!   [`section::SectionCache`].
//! - [`section::SystemPrompt`] / [`section::Section`] — named, ordered
//!   slices that compose into one [`crate::llm::Message::System`].
//! - [`section::SectionCache`] — `Mutex<HashMap>` memoizer keyed by
//!   section name. Stable sections go through it so the bytes the
//!   model sees are byte-identical turn after turn, keeping
//!   `DeepSeek`'s automatic prefix cache hot.
//! - [`static_prompt`] — the built-in static system-prompt manifest
//!   whose prose lives in `src/prompt/static/*.md`.
//! - [`context`] — boot-time, turn-snapshot, and per-call dynamic content
//!   (global/project `AGENTS.md`, `skills_index`, `env_info`).
//! - [`specialized`] — single-purpose prompts (title generation and
//!   compact summary) that do **not** share sections with the main
//!   iteration prompt.
//!
//! ## Static / dynamic discipline
//!
//! [`engine::PromptEngine::iteration_system`] emits built-in static
//! Markdown sections first, then turn-snapshot skill context, then
//! boot-time and run-stable dynamic sections. Stable rendered
//! sections go through [`section::SectionCache`] so the bytes sent to
//! the model stay stable until an explicit cache clear; `skills_index`
//! is rebuilt from the snapshot the agent refreshed before the turn.
//! Highly mutable context such as `MEMORY.md` is intentionally not part
//! of this system message; the agent injects it as transient user
//! context during request assembly.
//!
//! ## AGENTS.md overlay
//!
//! `~/.mandeven/AGENTS.md` is the per-user global overlay. In
//! addition, mandeven walks from the launch CWD upward and loads any
//! `AGENTS.md` files it finds, root-to-leaf. This keeps the runtime
//! convention aligned with Codex-style repository instructions while
//! still supporting a global user file.

pub mod context;
pub mod engine;
pub mod error;
pub mod section;
pub mod specialized;
pub mod static_prompt;

pub use context::AGENTS_FILENAME;
pub use engine::{PromptContext, PromptEngine};
pub use error::{Error, Result};
pub use section::{Section, SectionCache, SystemPrompt};
