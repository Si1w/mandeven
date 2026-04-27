//! Sandbox policy — the capability tier that gates write operations and
//! unsafe shell commands.
//!
//! Two tiers, deliberately coarse:
//!
//! - [`SandboxPolicy::ReadOnly`]: every write tool ([`crate::tools::file::FileWrite`],
//!   [`crate::tools::file::FileEdit`]) is rejected before it touches
//!   disk; the shell only runs commands on the
//!   [`super::commands`] allow-list.
//! - [`SandboxPolicy::WorkspaceWrite`] (default): writes are confined to
//!   the workspace canonical CWD by [`crate::tools::workspace`]; shell
//!   commands run subject to the existing deny patterns in
//!   [`crate::tools::shell`].
//!
//! Read access is intentionally **unbounded under both tiers** —
//! `file_read` and `grep` traverse the whole filesystem on either policy.
//! Reading and writing have asymmetric blast radius, and forcing reads
//! into the workspace would block legitimate cross-directory inspection
//! (looking at `~/Desktop/notes` while editing inside a project).
//!
//! The active policy is installed once at process startup via
//! [`SandboxPolicy::init`] (called from `main` after config load) and
//! retrieved by tools via [`SandboxPolicy::current`]. Policy is a
//! pure-data tier descriptor — the actual gating happens in helper
//! functions like [`ensure_writable_now`] and
//! [`super::commands::ensure_safe_command`], which take `policy` (or
//! consult the global directly) so unit tests can drive each tier
//! without touching the global.

use std::sync::RwLock;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tools::error::{Error, Result};

/// Tool capability tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPolicy {
    /// Read-only: write tools are rejected; shell is restricted to the
    /// known-safe allow-list.
    ReadOnly,

    /// Default. Writes confined to the workspace by [`crate::tools::workspace`];
    /// shell still subject to its deny patterns but otherwise unrestricted.
    #[default]
    WorkspaceWrite,
}

/// Optional `[sandbox]` block in `~/.mandeven/mandeven.toml`. Missing
/// section / missing field falls back to the default policy.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SandboxConfig {
    /// Active capability tier. Omit to take [`SandboxPolicy::default`].
    #[serde(default)]
    pub policy: SandboxPolicy,
}

/// Process-global active policy. Installed once at startup; readable
/// concurrently from every tool. `RwLock` (not `OnceLock`) so tests can
/// flip the tier without serial-test infrastructure.
static CURRENT: RwLock<SandboxPolicy> = RwLock::new(SandboxPolicy::WorkspaceWrite);

impl SandboxPolicy {
    /// Install the policy chosen at startup. Subsequent calls overwrite —
    /// last writer wins. Call exactly once from `main` after loading the
    /// user config.
    ///
    /// # Panics
    ///
    /// Panics if the lock guarding the global is poisoned, which can only
    /// happen if a previous holder panicked while writing — in that case
    /// the process is already in an unrecoverable state.
    pub fn init(policy: SandboxPolicy) {
        *CURRENT.write().expect("SandboxPolicy lock poisoned") = policy;
    }

    /// Currently-active policy. Defaults to [`SandboxPolicy::WorkspaceWrite`]
    /// when `init` was never called (library / test use).
    ///
    /// # Panics
    ///
    /// Panics if the lock guarding the global is poisoned. See
    /// [`SandboxPolicy::init`].
    #[must_use]
    pub fn current() -> SandboxPolicy {
        *CURRENT.read().expect("SandboxPolicy lock poisoned")
    }
}

/// Reject write operations under [`SandboxPolicy::ReadOnly`].
///
/// `tool` is folded into the error message so the model can tell which
/// invocation was rejected.
///
/// # Errors
///
/// Returns [`Error::Execution`] when `policy` is [`SandboxPolicy::ReadOnly`].
pub fn ensure_writable_now(tool: &'static str, policy: SandboxPolicy) -> Result<()> {
    match policy {
        SandboxPolicy::ReadOnly => Err(Error::Execution {
            tool: tool.to_string(),
            message: "sandbox policy is read_only; write tools are disabled".into(),
        }),
        SandboxPolicy::WorkspaceWrite => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only assert pure-function behaviour. The global `CURRENT` is shared
    // across the parallel test runner, so direct assertions on it would
    // race with any other test that flips the tier.

    #[test]
    fn read_only_blocks_writes() {
        let err = ensure_writable_now("file_write", SandboxPolicy::ReadOnly).unwrap_err();
        assert!(err.to_string().contains("read_only"));
    }

    #[test]
    fn workspace_write_allows_writes() {
        assert!(ensure_writable_now("file_write", SandboxPolicy::WorkspaceWrite).is_ok());
    }

    #[test]
    fn default_is_workspace_write() {
        assert_eq!(SandboxPolicy::default(), SandboxPolicy::WorkspaceWrite);
    }

    #[test]
    fn deserialise_snake_case_variants() {
        let read: SandboxPolicy = serde_json::from_str("\"read_only\"").unwrap();
        let write: SandboxPolicy = serde_json::from_str("\"workspace_write\"").unwrap();
        assert_eq!(read, SandboxPolicy::ReadOnly);
        assert_eq!(write, SandboxPolicy::WorkspaceWrite);
    }
}
