//! Workspace anchor — the canonicalised project root that scopes every
//! tool's filesystem access.
//!
//! Installed exactly once at startup via [`init`] from `main`, after
//! resolving the launch CWD. From then on, [`root`] returns the same
//! path on every call without further syscalls. Tools that need the
//! anchor (the writers and the search root) reach for [`root`] instead
//! of repeatedly calling `std::env::current_dir`.
//!
//! Why a global rather than a per-tool argument:
//!
//! - The workspace boundary is a *process-wide* fact, not a per-call
//!   parameter. Threading it through every tool registration adds
//!   noise without buying flexibility — there is exactly one workspace
//!   per mandeven session.
//! - The previous implementation re-syscalled `current_dir` +
//!   `canonicalize` on every tool invocation, which meant a runtime
//!   `set_current_dir` (or any library that does it) would silently
//!   relocate the boundary mid-session. `init`-once removes that class
//!   of bug.
//!
//! Also re-homes the path-resolution helpers
//! ([`resolve_for_write`] / [`resolve_for_read`] / [`lexical_normalize`]
//! / [`is_sensitive_name`]) that previously lived in `tools::workspace`.
//! They were never tool-specific — every helper just orbits the
//! workspace root.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use thiserror::Error;

/// Filenames that are never accepted as a write target, regardless of
/// directory. Lower-cased before comparison.
const SENSITIVE_FILENAMES: &[&str] = &[
    ".env",
    ".npmrc",
    ".pypirc",
    ".netrc",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Directory components that, if they appear anywhere in a write target,
/// poison the whole path. Avoids stomping on credential / VCS plumbing.
const SENSITIVE_DIR_COMPONENTS: &[&str] = &[".git", ".ssh", ".aws", ".gnupg"];

/// Errors surfaced by path resolution. Tool-layer callers wrap these in
/// their own [`crate::tools::error::Error::Execution`] with the tool name.
#[derive(Debug, Error)]
pub enum Error {
    /// Empty path string — the model passed `""` or whitespace only.
    #[error("path is empty")]
    EmptyPath,

    /// Lexically (or canonically) the path doesn't sit inside the
    /// workspace root.
    #[error("{path} is outside workspace {root}")]
    OutsideWorkspace {
        /// User-supplied path string.
        path: String,
        /// Active workspace root, lossy-stringified for diagnostics.
        root: String,
    },

    /// Filename matches [`SENSITIVE_FILENAMES`] or path crosses a
    /// [`SENSITIVE_DIR_COMPONENTS`] segment.
    #[error("writing sensitive path {0} is blocked")]
    SensitivePath(String),

    /// No existing ancestor of the target — even the workspace root
    /// went missing. Should be unreachable in practice.
    #[error("no existing parent for {0}")]
    NoExistingParent(String),

    /// Underlying filesystem syscall failure (canonicalize / metadata).
    /// `op` names the operation, `path` the subject.
    #[error("{op} {path}: {source}")]
    Io {
        /// Short operation label (`canonicalize`, `metadata`, …).
        op: &'static str,
        /// Path the syscall was attempted on.
        path: String,
        /// Underlying [`std::io::Error`].
        #[source]
        source: std::io::Error,
    },
}

/// Result alias for the [`workspace`](self) module.
pub type Result<T> = std::result::Result<T, Error>;

/// Process-global workspace root. Set by [`init`] from `main`. Tools
/// read it through [`root`].
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Install the workspace root for the rest of the process. Pass the
/// canonicalised launch CWD. Idempotent only in the trivial sense —
/// the second call is silently ignored (`OnceLock::set` returns `Err`,
/// which we drop).
///
/// Call exactly once, before any tool dispatches.
pub fn init(canonical_root: PathBuf) {
    let _ = ROOT.set(canonical_root);
}

/// Active workspace root.
///
/// When [`init`] was never called (library / test use), falls back to
/// the canonicalised process CWD. Panics in that fallback path if the
/// CWD is unreadable — under normal program flow `init` runs in `main`
/// before any tool, so the fallback is only exercised by direct unit
/// tests of helpers in this module.
///
/// # Panics
///
/// In the fallback branch only, when `std::env::current_dir` or
/// `std::fs::canonicalize` fails.
#[must_use]
pub fn root() -> PathBuf {
    ROOT.get().cloned().unwrap_or_else(|| {
        let cwd = std::env::current_dir().expect("CWD must be readable");
        std::fs::canonicalize(&cwd).expect("CWD must canonicalise")
    })
}

/// Fold `.` / `..` components without touching disk. Used as a
/// pre-canonicalisation check for paths that don't yet exist —
/// `tokio::fs::canonicalize` requires the target to exist, but
/// [`resolve_for_write`] needs to validate the *intended* location of a
/// file we're about to create.
#[must_use]
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Resolve a path destined to receive bytes.
///
/// Layered checks:
///
/// 1. Reject empty input.
/// 2. Anchor relative paths against [`root`].
/// 3. Lexically normalise and require the result to start with the
///    workspace root (catches `../escape` even before the file exists).
/// 4. Reject [`SENSITIVE_FILENAMES`] basenames and any path that crosses
///    a [`SENSITIVE_DIR_COMPONENTS`] segment.
/// 5. Resolve the longest existing prefix and re-check it canonicalises
///    inside the workspace (catches `dir/symlink-pointing-outside/file`).
///
/// # Errors
///
/// Returns [`Error`] when any layer rejects the path.
pub async fn resolve_for_write(raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        return Err(Error::EmptyPath);
    }

    let root_path = root();
    let input = Path::new(raw);
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root_path.join(input)
    };
    let normalized = lexical_normalize(&candidate);
    if !normalized.starts_with(&root_path) {
        return Err(Error::OutsideWorkspace {
            path: raw.into(),
            root: root_path.display().to_string(),
        });
    }
    if let Some(name) = normalized.file_name().and_then(|n| n.to_str())
        && is_sensitive_name(name)
    {
        return Err(Error::SensitivePath(raw.into()));
    }
    if normalized
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .any(|name| SENSITIVE_DIR_COMPONENTS.contains(&name))
    {
        return Err(Error::SensitivePath(raw.into()));
    }

    ensure_existing_prefix_inside_workspace(&root_path, &normalized).await?;
    Ok(normalized)
}

/// Resolve a path for a *read* operation. `None` / empty input →
/// workspace root. Returns the canonicalised absolute path. **No
/// workspace containment check** — reads are unbounded by design (see
/// the module docs in [`crate::security::policy`]).
///
/// # Errors
///
/// Returns [`Error::Io`] when the path can't be canonicalised (most
/// often: doesn't exist).
pub async fn resolve_for_read(raw: Option<&str>) -> Result<PathBuf> {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty());
    let Some(raw) = trimmed else {
        return Ok(root());
    };

    let input = Path::new(raw);
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root().join(input)
    };
    let normalized = lexical_normalize(&candidate);
    tokio::fs::canonicalize(&normalized)
        .await
        .map_err(|source| Error::Io {
            op: "canonicalize",
            path: raw.into(),
            source,
        })
}

/// True if `name` matches one of [`SENSITIVE_FILENAMES`]
/// (case-insensitive) or is a `.env.*` variant (`.env.local`,
/// `.env.production`, …).
#[must_use]
pub fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_FILENAMES.contains(&lower.as_str()) || lower.starts_with(".env.")
}

/// Walk up `path` to the nearest existing ancestor and confirm that
/// canonicalising it still lands inside `root`. Catches the
/// `dir/symlink-to-elsewhere/new-file` escape — [`lexical_normalize`]
/// can't see through symlinks, but canonicalising an existing prefix
/// does.
async fn ensure_existing_prefix_inside_workspace(root: &Path, path: &Path) -> Result<()> {
    let mut probe = path.to_path_buf();
    if !tokio::fs::try_exists(&probe)
        .await
        .map_err(|source| Error::Io {
            op: "exists",
            path: probe.display().to_string(),
            source,
        })?
    {
        probe = path.parent().unwrap_or(root).to_path_buf();
    }

    while !tokio::fs::try_exists(&probe)
        .await
        .map_err(|source| Error::Io {
            op: "exists",
            path: probe.display().to_string(),
            source,
        })?
    {
        if !probe.pop() {
            return Err(Error::NoExistingParent(path.display().to_string()));
        }
    }

    let canonical = tokio::fs::canonicalize(&probe)
        .await
        .map_err(|source| Error::Io {
            op: "canonicalize",
            path: probe.display().to_string(),
            source,
        })?;
    if !canonical.starts_with(root) {
        return Err(Error::OutsideWorkspace {
            path: probe.display().to_string(),
            root: root.display().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalize_collapses_dot_segments() {
        let p = lexical_normalize(Path::new("/a/./b/../c"));
        assert_eq!(p, PathBuf::from("/a/c"));
    }

    #[test]
    fn sensitive_name_rejects_dotenv_variants() {
        assert!(is_sensitive_name(".env"));
        assert!(is_sensitive_name(".env.local"));
        assert!(is_sensitive_name(".ENV"));
        assert!(is_sensitive_name("id_rsa"));
        assert!(!is_sensitive_name("env"));
        assert!(!is_sensitive_name("readme.md"));
    }

    #[tokio::test]
    async fn resolve_for_read_with_none_returns_root() {
        let resolved = resolve_for_read(None).await.unwrap();
        assert_eq!(resolved, root());
    }

    #[tokio::test]
    async fn resolve_for_read_allows_paths_outside_workspace() {
        // Either /etc/hosts canonicalises fine (most systems) or we
        // get a clean Io error — never an OutsideWorkspace error.
        if let Err(err) = resolve_for_read(Some("/etc/hosts")).await {
            assert!(!matches!(err, Error::OutsideWorkspace { .. }));
        }
    }

    #[tokio::test]
    async fn resolve_for_write_rejects_outside_workspace() {
        let err = resolve_for_write("/etc/passwd").await.unwrap_err();
        assert!(matches!(err, Error::OutsideWorkspace { .. }));
    }

    #[tokio::test]
    async fn resolve_for_write_rejects_sensitive_file() {
        let err = resolve_for_write(".env").await.unwrap_err();
        assert!(matches!(err, Error::SensitivePath(_)));
    }
}
