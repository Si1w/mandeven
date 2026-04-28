//! On-disk persistence for memory records and the derived user profile.
//!
//! Stores use one pretty JSON document per scope and atomic replace writes:
//! serialize to `*.tmp`, then rename over the canonical path. This mirrors the
//! task and cron stores and is enough for the current single-process runtime.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::{Error, Result};
use super::{Memory, ProfileFile, STORE_VERSION};

/// On-disk shape of `memories.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoreFile {
    /// Schema version. Values above [`STORE_VERSION`] are rejected.
    pub version: u32,

    /// Memory records in insertion order.
    #[serde(default)]
    pub memories: Vec<Memory>,
}

impl StoreFile {
    /// Construct an empty store file at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: STORE_VERSION,
            memories: Vec::new(),
        }
    }
}

impl Default for StoreFile {
    fn default() -> Self {
        Self::new()
    }
}

/// Async I/O wrapper around `memories.json`.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    tmp_path: PathBuf,
}

impl Store {
    /// Construct a store rooted at `<dir>/`.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let path = dir.join(super::MEMORY_STORE_FILENAME);
        let tmp_path = dir.join(format!("{}.tmp", super::MEMORY_STORE_FILENAME));
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

/// Async I/O wrapper around `profile.json`.
#[derive(Debug)]
pub struct ProfileStore {
    path: PathBuf,
    tmp_path: PathBuf,
}

impl ProfileStore {
    /// Construct a profile store rooted at `<global_memory_dir>/`.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let path = dir.join(super::PROFILE_STORE_FILENAME);
        let tmp_path = dir.join(format!("{}.tmp", super::PROFILE_STORE_FILENAME));
        Self { path, tmp_path }
    }

    /// Path to the canonical profile file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the profile file. Missing file returns `None`.
    ///
    /// # Errors
    ///
    /// Returns I/O, JSON, or unknown-version errors.
    pub async fn load(&self) -> Result<Option<ProfileFile>> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        let parsed: ProfileFile = serde_json::from_slice(&bytes)?;
        if parsed.version > STORE_VERSION {
            return Err(Error::InvalidStore(format!(
                "profile version {} is newer than this build supports ({STORE_VERSION})",
                parsed.version
            )));
        }
        Ok(Some(parsed))
    }

    /// Atomically replace the profile file.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn save(&self, file: &ProfileFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec_pretty(file)?;
        tokio::fs::write(&self.tmp_path, bytes).await?;
        tokio::fs::rename(&self.tmp_path, &self.path).await?;
        Ok(())
    }
}
