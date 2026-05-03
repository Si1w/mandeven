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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicWriteScope {
    /// User workspace files handled by `file_write` / `file_edit`.
    Workspace,
    /// Files under the mandeven data directory such as `timers.json`.
    GlobalDataDir,
    /// Per-project bucket files such as task Markdown.
    ProjectBucket,
    /// The single managed `MEMORY.md` carve-out.
    ManagedMemory,
}

/// Atomically replace `path` with UTF-8 text content.
///
/// # Errors
///
/// Returns I/O errors for parent creation, temp-file writes, rename
/// failures, or scope validation failures.
pub async fn atomic_write_text(path: &Path, content: &str, scope: AtomicWriteScope) -> Result<()> {
    validate_scope(path, scope)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "target path has no parent"))?;

    tokio::fs::create_dir_all(parent).await?;
    if scope == AtomicWriteScope::Workspace {
        ensure_parent_inside_workspace(parent).await?;
    }

    let tmp = temp_path(parent, path);
    tokio::fs::write(&tmp, content).await?;
    if let Err(err) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }
    Ok(())
}

fn validate_scope(path: &Path, scope: AtomicWriteScope) -> Result<()> {
    match scope {
        AtomicWriteScope::Workspace => {
            let root = workspace::root();
            let normalized = workspace::lexical_normalize(path);
            if !normalized.starts_with(&root) {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{} is outside workspace {}", path.display(), root.display()),
                ));
            }
            Ok(())
        }
        AtomicWriteScope::ManagedMemory => {
            let normalized = workspace::lexical_normalize(path);
            let memory = workspace::lexical_normalize(&crate::memory::default_memory_path());
            if normalized != memory {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{} is not the managed MEMORY.md", path.display()),
                ));
            }
            Ok(())
        }
        AtomicWriteScope::GlobalDataDir | AtomicWriteScope::ProjectBucket => Ok(()),
    }
}

async fn ensure_parent_inside_workspace(parent: &Path) -> Result<()> {
    let root = workspace::root();
    let canonical = tokio::fs::canonicalize(parent).await?;
    if canonical.starts_with(&root) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!(
            "{} resolves outside workspace {}",
            parent.display(),
            root.display()
        ),
    ))
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

        atomic_write_text(&path, "one", AtomicWriteScope::GlobalDataDir)
            .await
            .unwrap();
        atomic_write_text(&path, "two", AtomicWriteScope::GlobalDataDir)
            .await
            .unwrap();

        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "two");
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
