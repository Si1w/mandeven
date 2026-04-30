//! On-disk Dream state and run-log persistence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::error::{Error, Result};

/// Subdirectory under a project bucket holding Dream state.
pub const DREAM_SUBDIR: &str = "dream";

/// Filename holding cursor state.
pub const DREAM_STATE_FILENAME: &str = "state.json";

/// Filename used as a coarse per-project lock.
pub const DREAM_LOCK_FILENAME: &str = "lock";

/// Subdirectory holding short markdown run logs.
pub const DREAM_RUNS_SUBDIR: &str = "runs";

/// Current Dream state schema version.
pub const STORE_VERSION: u32 = 1;

/// On-disk Dream cursor state.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StateFile {
    /// Schema version.
    pub version: u32,
    /// Last run that successfully committed review cursors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<DateTime<Utc>>,
    /// Per-session review cursors, keyed by session UUID string.
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionCursor>,
}

impl StateFile {
    /// Empty state at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: STORE_VERSION,
            last_success_at: None,
            sessions: BTreeMap::new(),
        }
    }
}

/// Review cursor for one session.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SessionCursor {
    /// Last event sequence that Dream successfully reviewed and committed.
    pub reviewed_until_seq: u64,
    /// Highest event sequence observed at the beginning of the latest run.
    pub last_seen_seq: u64,
    /// Session `updated_at` as seen during the latest successful review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Dream store rooted at `<project_bucket>/dream/`.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    state_path: PathBuf,
    tmp_state_path: PathBuf,
    lock_path: PathBuf,
    runs_dir: PathBuf,
}

impl Store {
    /// Construct a store for the given project bucket.
    #[must_use]
    pub fn new(project_bucket: &Path) -> Self {
        let dir = project_bucket.join(DREAM_SUBDIR);
        Self {
            state_path: dir.join(DREAM_STATE_FILENAME),
            tmp_state_path: dir.join(format!("{DREAM_STATE_FILENAME}.tmp")),
            lock_path: dir.join(DREAM_LOCK_FILENAME),
            runs_dir: dir.join(DREAM_RUNS_SUBDIR),
            dir,
        }
    }

    /// Path to the canonical state file.
    #[must_use]
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Path to the coarse lock file.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Read cursor state. Missing file returns an empty state.
    ///
    /// # Errors
    ///
    /// Returns an error when the state file cannot be read or parsed.
    pub async fn load(&self) -> Result<StateFile> {
        let bytes = match tokio::fs::read(&self.state_path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StateFile::new());
            }
            Err(err) => return Err(Error::Io(err)),
        };
        let parsed: StateFile = serde_json::from_slice(&bytes)?;
        if parsed.version > STORE_VERSION {
            return Err(Error::InvalidExtraction(format!(
                "state version {} is newer than this build supports ({STORE_VERSION})",
                parsed.version
            )));
        }
        Ok(parsed)
    }

    /// Atomically replace cursor state.
    ///
    /// # Errors
    ///
    /// Returns an error when the state file cannot be written.
    pub async fn save(&self, file: &StateFile) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let bytes = serde_json::to_vec_pretty(file)?;
        tokio::fs::write(&self.tmp_state_path, bytes).await?;
        tokio::fs::rename(&self.tmp_state_path, &self.state_path).await?;
        Ok(())
    }

    /// Try to acquire the per-project Dream lock.
    ///
    /// Returns `Ok(None)` when another Dream run already owns the lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock directory or lock file cannot be written.
    pub async fn try_lock(&self, now: DateTime<Utc>) -> Result<Option<LockGuard>> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.lock_path)
            .await
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        file.write_all(now.to_rfc3339().as_bytes()).await?;
        file.flush().await?;
        Ok(Some(LockGuard {
            path: self.lock_path.clone(),
        }))
    }

    /// Write a short markdown run log.
    ///
    /// # Errors
    ///
    /// Returns an error when the run-log directory or file cannot be written.
    pub async fn write_run_log(&self, at: DateTime<Utc>, body: &str) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.runs_dir).await?;
        let name = format!("{}.md", at.format("%Y%m%dT%H%M%SZ"));
        let path = self.runs_dir.join(name);
        tokio::fs::write(&path, body.as_bytes()).await?;
        Ok(path)
    }
}

/// Held Dream lock.
pub struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    /// Release the lock file.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file exists but cannot be removed.
    pub async fn release(self) -> Result<()> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::Io(err)),
        }
    }
}
