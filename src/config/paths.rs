//! Filesystem layout for mandeven's per-user installation.
//!
//! Mirrors Claude Code's `~/.claude/` convention — see
//! `agent-examples/claude-code-analysis/src/utils/envUtils.ts:7` for
//! the original `getClaudeConfigHomeDir` and
//! `agent-examples/claude-code-analysis/src/utils/sessionStoragePortable.ts:325`
//! for `getProjectsDir` / `getProjectDir` / `sanitizePath`. The on-disk
//! layout is:
//!
//! ```text
//! ~/.mandeven/                            ← override with $MANDEVEN_HOME
//!   mandeven.toml                         ← global config
//!   AGENTS.md                             ← global agent instructions
//!   HEARTBEAT.md                          ← global heartbeat checklist
//!   cron/jobs.json                        ← global cron jobs
//!   projects/                             ← per-project session/task/memory bucket
//!     -Users-foo-projectA/
//!       <session-uuid>.jsonl
//!       tasks/
//!       memory/
//! ```
//!
//! Project-local overrides (e.g. a `<project>/.agents/AGENTS.md`
//! overlay on top of the global `AGENTS.md`) are intentionally **not**
//! implemented yet. The convention is reserved here so future code can
//! land it without renegotiating the layout.

use std::env;
use std::path::{Path, PathBuf};

/// Subdirectory of the user's home directory that holds every piece of
/// mandeven-managed state.
pub const HOME_SUBDIR: &str = ".mandeven";

/// Environment variable that overrides [`HOME_SUBDIR`] resolution. Set
/// it to an absolute path to point mandeven at a non-default install
/// (test isolation, multi-tenant setups, dotfiles managers).
pub const HOME_ENV_VAR: &str = "MANDEVEN_HOME";

/// Filename of the canonical config file inside [`HOME_SUBDIR`].
pub const CONFIG_FILENAME: &str = "mandeven.toml";

/// Subdirectory of [`HOME_SUBDIR`] holding per-project session/task/memory buckets.
pub const PROJECTS_SUBDIR: &str = "projects";

/// Reserved name for future project-local overrides. Not consumed yet
/// — see the module-level docstring.
pub const PROJECT_OVERRIDE_SUBDIR: &str = ".agents";

/// Maximum length of a sanitized path component. APFS / ext4 / NTFS all
/// cap individual components at 255 bytes; 200 leaves room for the
/// hash suffix Claude Code appends, and matches their constant so
/// bucket names produced by either implementation collide on common
/// short paths.
const MAX_SANITIZED_LENGTH: usize = 200;

/// Resolve the mandeven home directory.
///
/// Returns `$MANDEVEN_HOME` when set, else `$HOME/.mandeven`.
///
/// # Panics
///
/// Panics when neither `$MANDEVEN_HOME` is set nor `dirs::home_dir()`
/// can resolve a home directory. On every supported platform (macOS,
/// Linux, Windows) this is virtually impossible — the panic exists so a
/// truly broken environment surfaces a loud error rather than a silent
/// fallback to `./` that would scatter session files across cwd.
#[must_use]
pub fn home_dir() -> PathBuf {
    if let Some(override_path) = env::var_os(HOME_ENV_VAR) {
        return PathBuf::from(override_path);
    }
    dirs::home_dir()
        .map(|h| h.join(HOME_SUBDIR))
        .expect("cannot resolve home directory; set $MANDEVEN_HOME to override")
}

/// Path to the canonical config file under [`home_dir`].
#[must_use]
pub fn config_path() -> PathBuf {
    home_dir().join(CONFIG_FILENAME)
}

/// Path to the per-project bucket parent directory.
#[must_use]
pub fn projects_dir() -> PathBuf {
    home_dir().join(PROJECTS_SUBDIR)
}

/// Bucket directory for a specific project (typically
/// `std::env::current_dir()` captured once at process start).
#[must_use]
pub fn project_bucket(cwd: &Path) -> PathBuf {
    projects_dir().join(sanitize_path(&cwd.to_string_lossy()))
}

/// Sanitize a path so it survives as a single filesystem component on
/// every supported platform: replace any non-`[A-Za-z0-9]` byte with
/// `-`, then truncate + append a stable hash suffix when the result
/// would exceed [`MAX_SANITIZED_LENGTH`].
///
/// The hash uses DJB2 (the same algorithm Claude Code's SDK
/// implementation falls back to, see `simpleHash` in
/// `sessionStoragePortable.ts:295`) so two implementations of the same
/// rule produce identical bucket names for short paths and at least
/// hash-comparable names for long ones.
fn sanitize_path(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let prefix: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    format!("{prefix}-{:x}", djb2(name))
}

/// DJB2 string hash. Tiny, deterministic across Rust versions, no
/// extra dependency.
fn djb2(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(byte));
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_non_alphanumeric_with_hyphens() {
        assert_eq!(
            sanitize_path("/Users/foo/my project"),
            "-Users-foo-my-project"
        );
    }

    #[test]
    fn sanitize_short_path_is_not_hashed() {
        let s = sanitize_path("/short");
        assert!(!s.contains('x') || s == "-short");
        assert_eq!(s, "-short");
    }

    #[test]
    fn sanitize_long_path_truncates_and_appends_hash() {
        let long = "/".to_string() + &"a".repeat(MAX_SANITIZED_LENGTH * 2);
        let s = sanitize_path(&long);
        assert!(s.len() > MAX_SANITIZED_LENGTH);
        assert!(s.len() < MAX_SANITIZED_LENGTH + 32);
        assert!(s.contains('-'));
    }

    #[test]
    fn home_dir_honors_env_override() {
        // The env var is process-global. Rather than fight the rest of
        // the test suite, verify the override path explicitly without
        // touching unrelated tests.
        let raw = env::var_os(HOME_ENV_VAR);
        // SAFETY: single-threaded test, restored before assertions on
        // shared state. set_var/remove_var are unsafe in Rust 2024.
        unsafe { env::set_var(HOME_ENV_VAR, "/tmp/mandeven-test-home") };
        assert_eq!(home_dir(), PathBuf::from("/tmp/mandeven-test-home"));
        // Restore.
        match raw {
            Some(v) => unsafe { env::set_var(HOME_ENV_VAR, v) },
            None => unsafe { env::remove_var(HOME_ENV_VAR) },
        }
    }
}
