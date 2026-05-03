//! Atomic text-file replacement shared by tools and stores.
//!
//! The helper writes a unique temporary file next to the target and
//! renames it into place. On normal filesystems this prevents torn
//! partially-written target files, but it is intentionally not a
//! read-modify-write transaction; callers that mutate structured state
//! still need their own lock or actor around load/mutate/save.

use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::utils::workspace;

/// Logical owner of an atomic write.
///
/// This is an internal runtime boundary, not a model-facing concept.
/// Domain stores still validate their own schema before calling
/// [`atomic_write_text`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtomicWriteScope {
    /// User workspace files handled by `file_write` / `file_edit`.
    Workspace {
        /// Canonical workspace root.
        root: PathBuf,
    },
    /// Files under the mandeven data directory such as `timers.json`.
    GlobalDataDir {
        /// Data-directory root.
        root: PathBuf,
    },
    /// Per-project bucket files such as task Markdown.
    ProjectBucket {
        /// Project bucket or task-store root.
        root: PathBuf,
    },
    /// The single managed `MEMORY.md` carve-out.
    ManagedMemory {
        /// Exact `MEMORY.md` path this write is allowed to replace.
        path: PathBuf,
    },
}

/// Atomically replace `path` with UTF-8 text content.
///
/// # Errors
///
/// Returns I/O errors for parent creation, temp-file writes, rename
/// failures, or scope validation failures.
pub async fn atomic_write_text(path: &Path, content: &str, scope: AtomicWriteScope) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "target path has no parent"))?;

    prepare_scope(path, parent, &scope).await?;

    let tmp = temp_path(parent, path);
    tokio::fs::write(&tmp, content).await?;
    if let Err(err) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }
    Ok(())
}

async fn prepare_scope(path: &Path, parent: &Path, scope: &AtomicWriteScope) -> Result<()> {
    match scope {
        AtomicWriteScope::Workspace { root }
        | AtomicWriteScope::GlobalDataDir { root }
        | AtomicWriteScope::ProjectBucket { root } => {
            validate_root_scope(path, root)?;
            tokio::fs::create_dir_all(root).await?;
            ensure_existing_prefix_inside_root(root, path).await?;
            tokio::fs::create_dir_all(parent).await?;
            ensure_parent_inside_root(parent, root).await
        }
        AtomicWriteScope::ManagedMemory { path: expected } => {
            let normalized = workspace::lexical_normalize(path);
            let expected = workspace::lexical_normalize(expected);
            if normalized != expected {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{} is not the managed MEMORY.md", path.display()),
                ));
            }
            tokio::fs::create_dir_all(parent).await?;
            ensure_exact_parent(parent, &expected).await
        }
    }
}

fn validate_root_scope(path: &Path, root: &Path) -> Result<()> {
    let normalized = workspace::lexical_normalize(path);
    let root = workspace::lexical_normalize(root);
    if !normalized.starts_with(&root) {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is outside root {}", path.display(), root.display()),
        ));
    }
    Ok(())
}

async fn ensure_existing_prefix_inside_root(root: &Path, path: &Path) -> Result<()> {
    let mut probe = path.to_path_buf();
    if !tokio::fs::try_exists(&probe).await? {
        probe = path.parent().unwrap_or(root).to_path_buf();
    }

    while !tokio::fs::try_exists(&probe).await? {
        if !probe.pop() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!("no existing parent for {}", path.display()),
            ));
        }
    }

    let root = tokio::fs::canonicalize(root).await?;
    let canonical = tokio::fs::canonicalize(&probe).await?;
    if canonical.starts_with(&root) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!(
            "{} resolves outside root {}",
            probe.display(),
            root.display()
        ),
    ))
}

async fn ensure_parent_inside_root(parent: &Path, root: &Path) -> Result<()> {
    let root = tokio::fs::canonicalize(root).await?;
    let canonical = tokio::fs::canonicalize(parent).await?;
    if canonical.starts_with(&root) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!(
            "{} resolves outside root {}",
            parent.display(),
            root.display()
        ),
    ))
}

async fn ensure_exact_parent(parent: &Path, expected: &Path) -> Result<()> {
    let expected_parent = expected
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "managed path has no parent"))?;
    tokio::fs::create_dir_all(expected_parent).await?;
    let parent = tokio::fs::canonicalize(parent).await?;
    let expected_parent = tokio::fs::canonicalize(expected_parent).await?;
    if parent == expected_parent {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::PermissionDenied,
            "managed MEMORY.md parent mismatch",
        ))
    }
}

fn temp_path(parent: &Path, target: &Path) -> PathBuf {
    let filename = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("atomic-write");
    parent.join(format!(".{filename}.{}.tmp", Uuid::now_v7()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mandeven-atomic-{name}-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn atomic_write_replaces_file_contents() {
        let dir = tempdir("replace");
        let path = dir.join("state.json");

        atomic_write_text(
            &path,
            "one",
            AtomicWriteScope::GlobalDataDir { root: dir.clone() },
        )
        .await
        .unwrap();
        atomic_write_text(
            &path,
            "two",
            AtomicWriteScope::GlobalDataDir { root: dir.clone() },
        )
        .await
        .unwrap();

        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "two");
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
