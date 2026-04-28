//! On-disk persistence for project-local task lists.
//!
//! The store uses a single JSON document with atomic replace writes:
//! serialize to `tasks.json.tmp`, then `rename` over `tasks.json`.
//! This matches the existing cron store and is enough for the current
//! single-process runtime. The public manager keeps a process-local
//! mutex around load-mutate-save transactions so concurrent tool calls
//! do not race each other.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::{Error, Result};
use super::{STORE_VERSION, Task};

/// On-disk shape of `tasks.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoreFile {
    /// Schema version. Values above [`STORE_VERSION`] are rejected.
    pub version: u32,

    /// Highest numeric id ever assigned. Kept monotonic so deleting a
    /// task does not cause a later task to reuse its id.
    #[serde(default)]
    pub high_watermark: u64,

    /// Tasks in insertion order.
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl StoreFile {
    /// Construct an empty store file at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: STORE_VERSION,
            high_watermark: 0,
            tasks: Vec::new(),
        }
    }
}

impl Default for StoreFile {
    fn default() -> Self {
        Self::new()
    }
}

/// Async I/O wrapper around `tasks.json`.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    tmp_path: PathBuf,
}

impl Store {
    /// Construct a store rooted at `<project_bucket>/tasks/`.
    #[must_use]
    pub fn new(task_dir: &Path) -> Self {
        let path = task_dir.join(super::TASK_STORE_FILENAME);
        let tmp_path = task_dir.join(format!("{}.tmp", super::TASK_STORE_FILENAME));
        Self { path, tmp_path }
    }

    /// Path to the canonical store file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the store file. Missing file returns an empty store.
    ///
    /// # Errors
    ///
    /// Returns I/O, JSON, or unknown-version errors.
    pub async fn load(&self) -> Result<StoreFile> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoreFile::new());
            }
            Err(err) => return Err(Error::Io(err)),
        };
        let parsed: StoreFile = serde_json::from_slice(&bytes)?;
        if parsed.version > STORE_VERSION {
            return Err(Error::InvalidStore(format!(
                "store version {} is newer than this build supports ({STORE_VERSION})",
                parsed.version
            )));
        }
        Ok(parsed)
    }

    /// Atomically replace the store file with `file`.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn save(&self, file: &StoreFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec_pretty(file)?;
        tokio::fs::write(&self.tmp_path, bytes).await?;
        tokio::fs::rename(&self.tmp_path, &self.path).await?;
        Ok(())
    }
}
