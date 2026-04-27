//! Shared filesystem helpers for the `tools` module.
//!
//! Centralises three concerns that previously lived in [`super::file`]:
//!
//! 1. **Workspace anchor** — [`workspace_root`] returns the canonicalised
//!    process CWD. It's the writable boundary for [`super::file::FileWrite`]
//!    / [`super::file::FileEdit`] and the default search root for
//!    [`super::grep::Grep`].
//! 2. **Path normalisation** — [`lexical_normalize`] folds `.` / `..`
//!    components without touching disk, so callers can compare an
//!    intended path against the workspace prefix even when the file
//!    doesn't exist yet (write-then-create case).
//! 3. **Resolution kinds** — two flavours, asymmetric on purpose:
//!    - [`resolve_for_write`] enforces *workspace containment + sensitive
//!      filename rejection + symlink-prefix check*. Used by every tool
//!      that puts bytes on disk.
//!    - [`resolve_for_read`] only canonicalises. Used by tools that just
//!      look at files; deliberately unbounded so the model can search /
//!      inspect anywhere readable by the user, including
//!      `~/Desktop/notes` while editing a project.
//!
//! The asymmetry mirrors codex's `SandboxPolicy::WorkspaceWrite` shape:
//! reads are unbounded, writes confined.

use std::path::{Component, Path, PathBuf};

use super::error::{Error, Result};

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

/// Canonical workspace root — the process CWD with all symlinks resolved.
///
/// # Errors
///
/// Returns [`Error::Execution`] when the CWD cannot be read or canonicalised.
pub fn workspace_root() -> Result<PathBuf> {
    let cwd =
        std::env::current_dir().map_err(|e| exec("workspace", format!("current_dir: {e}")))?;
    std::fs::canonicalize(&cwd)
        .map_err(|e| exec("workspace", format!("canonicalize {}: {e}", cwd.display())))
}

/// Fold `.` / `..` components without touching disk.
///
/// Used as a pre-canonicalisation check for paths that don't yet exist —
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
/// 2. Anchor relative paths against [`workspace_root`].
/// 3. Lexically normalise and require the result to start with the
///    workspace root (catches `../escape` even before the file exists).
/// 4. Reject [`SENSITIVE_FILENAMES`] basenames and any path that crosses
///    a [`SENSITIVE_DIR_COMPONENTS`] segment.
/// 5. Resolve the longest existing prefix and re-check it canonicalises
///    inside the workspace (catches `dir/symlink-pointing-outside/file`).
///
/// `tool` is folded into every error message so the model can attribute
/// the failure to its own tool name.
///
/// # Errors
///
/// Returns [`Error::Execution`] when any layer rejects the path.
pub async fn resolve_for_write(tool: &'static str, raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        return Err(exec(tool, "path is empty"));
    }

    let root = workspace_root()?;
    let input = Path::new(raw);
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let normalized = lexical_normalize(&candidate);
    if !normalized.starts_with(&root) {
        return Err(exec(
            tool,
            format!("{raw} is outside workspace {}", root.display()),
        ));
    }
    if let Some(name) = normalized.file_name().and_then(|n| n.to_str())
        && is_sensitive_name(name)
    {
        return Err(exec(
            tool,
            format!("writing sensitive path {raw} is blocked"),
        ));
    }
    if normalized
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .any(|name| SENSITIVE_DIR_COMPONENTS.contains(&name))
    {
        return Err(exec(
            tool,
            format!("writing sensitive path {raw} is blocked"),
        ));
    }

    ensure_existing_prefix_inside_workspace(tool, &root, &normalized).await?;
    Ok(normalized)
}

/// Resolve a path for a *read* operation. Empty input → workspace root.
/// Returns the canonicalised absolute path. **No workspace containment
/// check** — reads are unbounded by design (see module docs).
///
/// # Errors
///
/// Returns [`Error::Execution`] when the path cannot be canonicalised
/// (most often: doesn't exist).
pub async fn resolve_for_read(tool: &'static str, raw: Option<&str>) -> Result<PathBuf> {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty());
    let Some(raw) = trimmed else {
        return workspace_root();
    };

    let input = Path::new(raw);
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        workspace_root()?.join(input)
    };
    let normalized = lexical_normalize(&candidate);
    tokio::fs::canonicalize(&normalized)
        .await
        .map_err(|e| exec(tool, format!("{raw}: {e}")))
}

/// True if `name` matches one of [`SENSITIVE_FILENAMES`] (case-insensitive)
/// or is a `.env.*` variant (`.env.local`, `.env.production`, …).
#[must_use]
pub fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_FILENAMES.contains(&lower.as_str()) || lower.starts_with(".env.")
}

/// Walk up `path` to the nearest existing ancestor and confirm that
/// canonicalising it still lands inside `root`. Catches the
/// `dir/symlink-to-elsewhere/new-file` escape — `lexical_normalize`
/// can't see through symlinks, but canonicalising an existing prefix
/// does.
async fn ensure_existing_prefix_inside_workspace(
    tool: &'static str,
    root: &Path,
    path: &Path,
) -> Result<()> {
    let mut probe = path.to_path_buf();
    if !tokio::fs::try_exists(&probe)
        .await
        .map_err(|e| exec(tool, format!("{}: {e}", probe.display())))?
    {
        probe = path.parent().unwrap_or(root).to_path_buf();
    }

    while !tokio::fs::try_exists(&probe)
        .await
        .map_err(|e| exec(tool, format!("{}: {e}", probe.display())))?
    {
        if !probe.pop() {
            return Err(exec(
                tool,
                format!("no existing parent for {}", path.display()),
            ));
        }
    }

    let canonical = tokio::fs::canonicalize(&probe)
        .await
        .map_err(|e| exec(tool, format!("canonicalize {}: {e}", probe.display())))?;
    if !canonical.starts_with(root) {
        return Err(exec(
            tool,
            format!(
                "{} resolves outside workspace {}",
                probe.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

fn exec(tool: &'static str, message: impl Into<String>) -> Error {
    Error::Execution {
        tool: tool.to_string(),
        message: message.into(),
    }
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
    async fn resolve_for_read_with_none_returns_workspace_root() {
        let root = workspace_root().unwrap();
        let resolved = resolve_for_read("test", None).await.unwrap();
        assert_eq!(resolved, root);
    }

    #[tokio::test]
    async fn resolve_for_read_allows_paths_outside_workspace() {
        // /etc/hosts is canonical and outside any reasonable workspace.
        let resolved = resolve_for_read("test", Some("/etc/hosts")).await;
        // Either the file exists (most systems) and resolves OK, or it
        // doesn't and we get a clean canonicalize error — never a
        // workspace-containment error.
        if let Err(err) = resolved {
            assert!(!err.to_string().contains("outside workspace"));
        }
    }

    #[tokio::test]
    async fn resolve_for_write_rejects_outside_workspace() {
        let err = resolve_for_write("test", "/etc/passwd")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside workspace"));
    }

    #[tokio::test]
    async fn resolve_for_write_rejects_sensitive_file() {
        let err = resolve_for_write("test", ".env")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("sensitive"));
    }
}
