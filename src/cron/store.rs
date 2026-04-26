//! On-disk persistence for cron job definitions and runtime state.
//!
//! Single-file layout (`<data_dir>/cron/jobs.json`) with atomic writes:
//! serialize the whole document, write to `jobs.json.tmp`, then
//! `rename` over `jobs.json`. Mirrors [`crate::session::Manager`]'s
//! pattern — a POSIX rename is atomic, so a concurrent reader either
//! sees the previous file or the new one, never a half-written one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::{Error, Result};
use super::types::{CronJob, STORE_VERSION};

/// On-disk shape of `jobs.json`. Wraps the job list with a version
/// integer so future format changes can branch on it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoreFile {
    /// Schema version. Equal to [`STORE_VERSION`] for files this
    /// build can read; higher versions trigger an
    /// [`Error::InvalidStore`] rather than silent best-effort
    /// parsing.
    pub version: u32,

    /// All jobs in insertion order.
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            jobs: Vec::new(),
        }
    }
}

/// Async I/O wrapper around `jobs.json`.
///
/// `Store` is intentionally stateless beyond paths — every load
/// re-reads the file. The engine owns an in-memory copy and calls
/// [`Store::save`] after each mutation; `Store` itself does not
/// cache.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    tmp_path: PathBuf,
}

impl Store {
    /// Construct a store rooted at `<data_dir>/cron/`. The directory
    /// is created on first [`Store::save`] — bare construction is
    /// pure path manipulation and never touches the filesystem.
    #[must_use]
    pub fn new(cron_dir: &Path) -> Self {
        let path = cron_dir.join(super::CRON_STORE_FILENAME);
        let tmp_path = cron_dir.join(format!("{}.tmp", super::CRON_STORE_FILENAME));
        Self { path, tmp_path }
    }

    /// Path to the canonical `jobs.json`. Useful for error messages
    /// and tests; callers should prefer [`Store::load`] / [`Store::save`].
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the store file. Missing file deserializes to an empty
    /// [`StoreFile`] — first-boot defaults rather than an error.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] on read failures other than `NotFound`.
    /// - [`Error::Json`] when the file is unreadable JSON.
    /// - [`Error::InvalidStore`] when the file has an unknown
    ///   schema version.
    pub async fn load(&self) -> Result<StoreFile> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoreFile::default());
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

    /// Atomically replace the store file with `file`'s contents.
    ///
    /// Creates the parent directory on demand so new installs work
    /// without a pre-built filesystem layout.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] on directory or file write failures.
    /// - [`Error::Json`] when serialization fails (shouldn't happen
    ///   in practice — the data shape is fixed).
    pub async fn save(&self, file: &StoreFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec_pretty(file)?;
        tokio::fs::write(&self.tmp_path, &bytes).await?;
        tokio::fs::rename(&self.tmp_path, &self.path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::cron::Schedule;

    fn ts(rfc: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample_job() -> CronJob {
        CronJob::new(
            "sample".into(),
            Schedule::cron("0 9 * * *").unwrap(),
            "Generate yesterday's commit summary.".into(),
            ts("2026-04-25T00:00:00Z"),
        )
    }

    #[tokio::test]
    async fn load_returns_default_when_file_missing() {
        let dir = tempdir();
        let store = Store::new(&dir);
        let file = store.load().await.unwrap();
        assert_eq!(file.version, STORE_VERSION);
        assert!(file.jobs.is_empty());
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let dir = tempdir();
        let store = Store::new(&dir);
        let mut file = StoreFile::default();
        file.jobs.push(sample_job());
        store.save(&file).await.unwrap();

        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.jobs.len(), 1);
        assert_eq!(loaded.jobs[0].name, "sample");
        assert_eq!(loaded.jobs[0].schedule.kind(), "cron");
    }

    #[tokio::test]
    async fn save_creates_missing_parent_directory() {
        let dir = tempdir().join("nested").join("cron");
        let store = Store::new(&dir);
        store.save(&StoreFile::default()).await.unwrap();
        assert!(store.path().exists());
    }

    #[tokio::test]
    async fn load_rejects_unknown_future_version() {
        let dir = tempdir();
        let store = Store::new(&dir);
        // Hand-write a future-version file.
        if let Some(parent) = store.path().parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(store.path(), br#"{"version":999,"jobs":[]}"#)
            .await
            .unwrap();
        let err = store.load().await.unwrap_err();
        assert!(matches!(err, Error::InvalidStore(_)));
    }

    /// Allocate a fresh temp dir under `target/` so test runs don't
    /// race each other. Avoids the `tempfile` crate dependency for
    /// what is essentially a path-mint-and-mkdir.
    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-cron-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
