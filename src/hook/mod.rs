//! Hook — shell-command extension points fired at agent lifecycle
//! events.
//!
//! Inspired by Claude Code's hook system (see
//! `agent-examples/claude-code-analysis/src/utils/hooks.ts` and
//! `src/types/hooks.ts`), pared down to what mandeven needs today:
//!
//! - 7 events ([`HookEvent`]) — `UserPromptSubmit`, `Pre/PostToolUse`,
//!   `SessionStart`, `Stop`, and `Pre/PostCompact`.
//! - 1 hook type — shell `command`. Claude Code also has `prompt`,
//!   `agent`, `http`; v1 doesn't. Future use cases can wrap a shell
//!   command around `curl` etc.
//! - Sync execution only. Claude Code supports `async: true`
//!   (background execution); v1 always blocks.
//! - Stdout decision protocol matching Claude's: `{"decision":
//!   "block", "reason": "..."}` plus exit-code-based blocking gated
//!   by per-hook `block_on_nonzero_exit`.
//!
//! ## Wire protocol
//!
//! Each hook command is invoked via `sh -c <command>` and receives:
//!
//! - **stdin**: the event payload as a JSON object, terminated with
//!   `\n`. Shape varies by event — see [`HookEvent`] docstrings.
//! - **env**:
//!     - `MANDEVEN_PROJECT_DIR` — `~/.mandeven` (or `$MANDEVEN_HOME`)
//!     - `MANDEVEN_SESSION_ID` — current session UUID
//!     - `MANDEVEN_CWD` — agent's launch directory
//!     - `MANDEVEN_HOOK_EVENT` — event name as a string
//! - **cwd**: the agent's launch directory (matches Claude's
//!   `getOriginalCwd()` semantics).
//!
//! ## Configuration
//!
//! Engine on/off lives in `mandeven.toml`:
//!
//! ```toml
//! [agent.hook]
//! enabled = true
//! ```
//!
//! Hook definitions live in `~/.mandeven/hooks.json` so they can be
//! changed without editing the toml. The v1 engine snapshots the file
//! at boot; hot reload needs an explicit reload path or file watcher.

pub mod engine;
pub mod error;
pub mod types;

pub use engine::{HookEngine, HookFireResult, HookOutcome};
pub use error::{Error, Result};
pub use types::{CommandHook, HookEvent, HookFile, HookMatcher};

use serde::{Deserialize, Serialize};

/// Filename inside [`crate::config::home_dir`] holding hook
/// definitions.
pub const HOOKS_FILENAME: &str = "hooks.json";

/// Default per-hook timeout when the user doesn't specify one. 30s
/// matches Claude Code's `TOOL_HOOK_EXECUTION_TIMEOUT_MS = 30_000`.
pub const HOOK_TIMEOUT_SECS_DEFAULT: u64 = 30;

/// User-tunable knob for the hook subsystem.
///
/// `enabled = false` skips loading `hooks.json` and bypasses every
/// `fire()` call (returns an empty result). Same shape as
/// `[agent.skill]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookConfig {
    /// When `false`, the engine constructs as a no-op: no scan, no
    /// fire. Default `true` so dropping a `hooks.json` into the
    /// data directory works without editing config.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
        }
    }
}

fn default_enabled() -> bool {
    true
}
