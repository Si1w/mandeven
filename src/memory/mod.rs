//! User-authored durable memory stored as one editable `MEMORY.md`.
//!
//! Memory is intentionally not a record database. The runtime reads a
//! single Markdown file from the mandeven data directory and injects it
//! as transient user context on each model request. Model-side updates
//! go through normal file editing, with a narrow write carve-out for
//! this exact file plus validation before the edit is committed.

pub mod error;

pub use error::{Error, Result};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename of the global user memory file under `~/.mandeven/`.
pub const MEMORY_FILENAME: &str = "MEMORY.md";

/// Default byte cap for loading and writing `MEMORY.md`.
pub const DEFAULT_MAX_BYTES: usize = 25_000;

/// Default line cap for loading and writing `MEMORY.md`.
pub const DEFAULT_MAX_LINES: usize = 200;

const EMPTY_MEMORY: &str = "# Memory\n\n";

/// User-tunable knobs for durable memory.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryConfig {
    /// When `false`, `MEMORY.md` is neither created nor injected.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Maximum bytes loaded from `MEMORY.md` into request context.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,

    /// Maximum lines loaded from `MEMORY.md` into request context.
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            max_bytes: default_max_bytes(),
            max_lines: default_max_lines(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_max_bytes() -> usize {
    DEFAULT_MAX_BYTES
}

fn default_max_lines() -> usize {
    DEFAULT_MAX_LINES
}

/// Handle for the single global memory file.
#[derive(Debug, Clone)]
pub struct Manager {
    path: PathBuf,
}

impl Manager {
    /// Build a manager rooted at the mandeven data directory.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: memory_path(data_dir),
        }
    }

    /// Absolute path to the managed memory file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create an empty `MEMORY.md` if memory is enabled and the file is absent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the parent directory or file cannot be created.
    pub async fn ensure_exists(&self, cfg: &MemoryConfig) -> Result<()> {
        if !cfg.enabled {
            return Ok(());
        }
        if tokio::fs::try_exists(&self.path).await? {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&self.path, EMPTY_MEMORY).await?;
        Ok(())
    }

    /// Render `MEMORY.md` as a transient user-context message body.
    ///
    /// Missing memory returns `Ok(None)`. An empty file still produces
    /// a small context message with the path so the `memorize` skill
    /// knows where to edit.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for unreadable files and [`Error::UnsafeContent`]
    /// for memory content that violates validation limits.
    pub async fn render_user_context(&self, cfg: &MemoryConfig) -> Result<Option<String>> {
        if !cfg.enabled {
            return Ok(None);
        }
        let raw = match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        validate_memory_markdown_with_limits(&raw, cfg.max_bytes, cfg.max_lines)?;

        let content = raw.trim();
        if content.is_empty() || content == "# Memory" {
            return Ok(Some(format!(
                "# User Memory\n\nPath: {}\n\n`{MEMORY_FILENAME}` is currently empty.",
                self.path.display()
            )));
        }
        Ok(Some(format!(
            "# User Memory\n\nPath: {}\n\n{}",
            self.path.display(),
            content
        )))
    }
}

/// Resolve the global `MEMORY.md` path under a data directory.
#[must_use]
pub fn memory_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MEMORY_FILENAME)
}

/// Resolve the default global `MEMORY.md` path.
#[must_use]
pub fn default_memory_path() -> PathBuf {
    memory_path(&crate::config::paths::home_dir())
}

/// Return true when `path` is the exact managed `MEMORY.md` path.
#[must_use]
pub fn is_managed_memory_path(path: &Path) -> bool {
    normalize(path) == normalize(&default_memory_path())
}

/// Validate content before it is used as memory.
///
/// # Errors
///
/// Returns [`Error::UnsafeContent`] for oversized files, invisible
/// control characters, and obvious secret-bearing lines.
pub fn validate_memory_markdown(content: &str) -> Result<()> {
    validate_memory_markdown_with_limits(content, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES)
}

fn validate_memory_markdown_with_limits(
    content: &str,
    max_bytes: usize,
    max_lines: usize,
) -> Result<()> {
    if content.len() > max_bytes {
        return Err(Error::UnsafeContent(format!(
            "{MEMORY_FILENAME} is {} bytes; limit is {max_bytes}",
            content.len()
        )));
    }
    let line_count = content.lines().count();
    if line_count > max_lines {
        return Err(Error::UnsafeContent(format!(
            "{MEMORY_FILENAME} is {line_count} lines; limit is {max_lines}"
        )));
    }
    for ch in content.chars() {
        if ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t' {
            return Err(Error::UnsafeContent(
                "control characters are not allowed".to_string(),
            ));
        }
    }
    reject_secret_patterns(content)?;
    Ok(())
}

fn reject_secret_patterns(content: &str) -> Result<()> {
    const PATTERNS: &[&str] = &[
        "api_key",
        "apikey",
        "access_token",
        "auth_token",
        "bearer ",
        "client_secret",
        "password",
        "private key",
        "secret_key",
        "ssh-rsa",
        "token=",
    ];

    for line in content.lines() {
        let lower = line.to_ascii_lowercase();
        if PATTERNS.iter().any(|pattern| lower.contains(pattern)) {
            return Err(Error::UnsafeContent(
                "line contains a disallowed secret-like pattern".to_string(),
            ));
        }
    }
    Ok(())
}

fn normalize(path: &Path) -> PathBuf {
    crate::utils::workspace::lexical_normalize(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tempdir() -> PathBuf {
        let dir = env::temp_dir().join(format!("mandeven-memory-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn render_user_context_includes_empty_memory_path() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        manager
            .ensure_exists(&MemoryConfig::default())
            .await
            .unwrap();

        let rendered = manager
            .render_user_context(&MemoryConfig::default())
            .await
            .unwrap();

        let rendered = rendered.unwrap();
        assert!(rendered.contains("Path:"));
        assert!(rendered.contains("currently empty"));
    }

    #[tokio::test]
    async fn render_user_context_wraps_memory_content() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        tokio::fs::write(manager.path(), "# Memory\n\n- Prefers concise replies.\n")
            .await
            .unwrap();

        let rendered = manager
            .render_user_context(&MemoryConfig::default())
            .await
            .unwrap()
            .unwrap();

        assert!(rendered.starts_with("# User Memory"));
        assert!(rendered.contains("- Prefers concise replies."));
    }

    #[test]
    fn validation_rejects_secret_like_lines() {
        let err = validate_memory_markdown("- api_key = abc").unwrap_err();
        assert!(err.to_string().contains("secret-like"));
    }

    #[test]
    fn validation_rejects_oversized_memory() {
        let content = "x".repeat(DEFAULT_MAX_BYTES + 1);
        let err = validate_memory_markdown(&content).unwrap_err();
        assert!(err.to_string().contains("limit"));
    }
}
