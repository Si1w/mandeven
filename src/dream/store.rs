//! On-disk Dream state and run-log persistence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::error::{Error, Result};

/// Subdirectory under a project bucket holding Dream state.
pub const DREAM_SUBDIR: &str = "dream";

/// Filename holding cursor state.
pub const DREAM_STATE_FILENAME: &str = "state.json";

/// Filename whose mtime records the last successful consolidation.
pub const DREAM_LOCK_FILENAME: &str = "lock.json";

/// Filename used as the active per-project Dream run lock.
const DREAM_ACTIVE_LOCK_FILENAME: &str = "lock.active.json";

/// Legacy lock filename used before lock metadata and stale recovery existed.
const LEGACY_DREAM_LOCK_FILENAME: &str = "lock";

/// Subdirectory holding short markdown run logs.
pub const DREAM_RUNS_SUBDIR: &str = "runs";

/// Current Dream state schema version.
pub const STORE_VERSION: u32 = 1;

/// Current Dream lock schema version.
const LOCK_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LockFile {
    version: u32,
    acquired_at: DateTime<Utc>,
    owner: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ConsolidationMarker {
    version: u32,
    consolidated_at: DateTime<Utc>,
}

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
    active_lock_path: PathBuf,
    legacy_lock_path: PathBuf,
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
            active_lock_path: dir.join(DREAM_ACTIVE_LOCK_FILENAME),
            legacy_lock_path: dir.join(LEGACY_DREAM_LOCK_FILENAME),
            runs_dir: dir.join(DREAM_RUNS_SUBDIR),
            dir,
        }
    }

    /// Path to the canonical state file.
    #[must_use]
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Path to the consolidation marker file.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Return the last successful consolidation time from `lock.json` mtime.
    ///
    /// # Errors
    ///
    /// Returns an error when lock metadata cannot be read.
    pub async fn last_consolidated_at(&self) -> Result<Option<DateTime<Utc>>> {
        match tokio::fs::metadata(&self.lock_path).await {
            Ok(metadata) => {
                let modified = metadata.modified().map_err(Error::Io)?;
                Ok(Some(DateTime::<Utc>::from(modified)))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::Io(err)),
        }
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
    /// Returns `Ok(None)` when another Dream run already owns a non-stale lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock directory or lock file cannot be written.
    pub async fn try_lock(
        &self,
        now: DateTime<Utc>,
        stale_after_secs: u64,
    ) -> Result<Option<LockGuard>> {
        tokio::fs::create_dir_all(&self.dir).await?;

        if !self.remove_stale_legacy_lock(now, stale_after_secs).await? {
            return Ok(None);
        }

        match self.create_active_lock(now).await {
            Ok(lock) => Ok(Some(lock)),
            Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if !self.remove_stale_active_lock(now, stale_after_secs).await? {
                    return Ok(None);
                }
                match self.create_active_lock(now).await {
                    Ok(lock) => Ok(Some(lock)),
                    Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        Ok(None)
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn create_active_lock(&self, now: DateTime<Utc>) -> Result<LockGuard> {
        let body = serde_json::to_vec_pretty(&LockFile {
            version: LOCK_VERSION,
            acquired_at: now,
            owner: format!("pid:{}:{}", std::process::id(), uuid::Uuid::now_v7()),
        })?;
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.active_lock_path)
            .await
        {
            Ok(file) => file,
            Err(err) => return Err(Error::Io(err)),
        };
        if let Err(err) = file.write_all(&body).await {
            let _ = tokio::fs::remove_file(&self.active_lock_path).await;
            return Err(Error::Io(err));
        }
        if let Err(err) = file.flush().await {
            let _ = tokio::fs::remove_file(&self.active_lock_path).await;
            return Err(Error::Io(err));
        }
        Ok(LockGuard {
            active_path: self.active_lock_path.clone(),
            marker_path: self.lock_path.clone(),
        })
    }

    async fn remove_stale_active_lock(
        &self,
        now: DateTime<Utc>,
        stale_after_secs: u64,
    ) -> Result<bool> {
        let bytes = match tokio::fs::read(&self.active_lock_path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(err) => return Err(Error::Io(err)),
        };
        let acquired_at = serde_json::from_slice::<LockFile>(&bytes)
            .ok()
            .filter(|lock| lock.version <= LOCK_VERSION)
            .map(|lock| lock.acquired_at);
        self.remove_lock_if_stale(&self.active_lock_path, acquired_at, now, stale_after_secs)
            .await
    }

    async fn remove_stale_legacy_lock(
        &self,
        now: DateTime<Utc>,
        stale_after_secs: u64,
    ) -> Result<bool> {
        let bytes = match tokio::fs::read(&self.legacy_lock_path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(err) => return Err(Error::Io(err)),
        };
        let acquired_at = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw.trim()).ok())
            .map(|at| at.with_timezone(&Utc));
        self.remove_lock_if_stale(&self.legacy_lock_path, acquired_at, now, stale_after_secs)
            .await
    }

    async fn remove_lock_if_stale(
        &self,
        path: &Path,
        acquired_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        stale_after_secs: u64,
    ) -> Result<bool> {
        let stale = match acquired_at {
            Some(acquired_at) => is_stale(acquired_at, now, stale_after_secs),
            None => match tokio::fs::metadata(path).await {
                Ok(metadata) => metadata
                    .modified()
                    .is_ok_and(|modified| system_time_is_stale(modified, now, stale_after_secs)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(err) => return Err(Error::Io(err)),
            },
        };
        if !stale {
            return Ok(false);
        }
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(err) => Err(Error::Io(err)),
        }
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

fn is_stale(acquired_at: DateTime<Utc>, now: DateTime<Utc>, stale_after_secs: u64) -> bool {
    let Ok(elapsed) = (now - acquired_at).to_std() else {
        return false;
    };
    elapsed.as_secs() >= stale_after_secs
}

fn system_time_is_stale(modified: SystemTime, now: DateTime<Utc>, stale_after_secs: u64) -> bool {
    is_stale(DateTime::<Utc>::from(modified), now, stale_after_secs)
}

/// Held Dream lock.
pub struct LockGuard {
    active_path: PathBuf,
    marker_path: PathBuf,
}

impl LockGuard {
    /// Mark consolidation as successful and release the active lock.
    ///
    /// `lock.json` is intentionally retained; its file mtime is the scheduling
    /// cursor for the next consolidation scan.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker or active lock cannot be updated.
    pub async fn commit(self, at: DateTime<Utc>) -> Result<()> {
        let body = serde_json::to_vec_pretty(&ConsolidationMarker {
            version: LOCK_VERSION,
            consolidated_at: at,
        })?;
        tokio::fs::write(&self.marker_path, body).await?;
        self.release().await
    }

    /// Release the active lock file without updating consolidation mtime.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file exists but cannot be removed.
    pub async fn release(self) -> Result<()> {
        match tokio::fs::remove_file(&self.active_path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::Io(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mandeven-dream-store-{label}-{}",
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn try_lock_reclaims_stale_json_lock() {
        let dir = tempdir("stale-json");
        let store = Store::new(&dir);
        tokio::fs::create_dir_all(&store.dir).await.unwrap();
        let now = Utc::now();
        let stale = LockFile {
            version: LOCK_VERSION,
            acquired_at: now - Duration::seconds(120),
            owner: "dead-process".to_string(),
        };
        tokio::fs::write(&store.active_lock_path, serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();

        let lock = store.try_lock(now, 60).await.unwrap();

        assert!(lock.is_some());
        lock.unwrap().release().await.unwrap();
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn try_lock_blocks_on_fresh_json_lock() {
        let dir = tempdir("fresh-json");
        let store = Store::new(&dir);
        let now = Utc::now();
        let lock = store.try_lock(now, 60).await.unwrap().unwrap();

        let second = store
            .try_lock(now + Duration::seconds(10), 60)
            .await
            .unwrap();

        assert!(second.is_none());
        lock.release().await.unwrap();
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn try_lock_reclaims_stale_legacy_lock() {
        let dir = tempdir("stale-legacy");
        let store = Store::new(&dir);
        tokio::fs::create_dir_all(&store.dir).await.unwrap();
        let now = Utc::now();
        tokio::fs::write(
            &store.legacy_lock_path,
            (now - Duration::seconds(120)).to_rfc3339(),
        )
        .await
        .unwrap();

        let lock = store.try_lock(now, 60).await.unwrap();

        assert!(lock.is_some());
        assert!(!store.legacy_lock_path.exists());
        lock.unwrap().release().await.unwrap();
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn commit_leaves_marker_mtime_and_releases_active_lock() {
        let dir = tempdir("commit-marker");
        let store = Store::new(&dir);
        let now = Utc::now();
        let lock = store.try_lock(now, 60).await.unwrap().unwrap();

        lock.commit(now).await.unwrap();

        assert!(store.lock_path.exists());
        assert!(!store.active_lock_path.exists());
        assert!(store.last_consolidated_at().await.unwrap().is_some());
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
