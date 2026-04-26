//! Errors surfaced by the cron module.

use thiserror::Error;

use super::schedule::ScheduleError;

/// Result alias used across cron — `Result<T, cron::Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Operations a cron caller can fail at.
#[derive(Debug, Error)]
pub enum Error {
    /// Job lookup failed — the supplied id is not in the store.
    #[error("cron job {0:?} not found")]
    JobNotFound(String),

    /// Tried to register a job whose id collides with an existing one.
    /// Should only happen when callers reuse [`super::types::CronJob::id`]
    /// rather than minting via [`super::types::CronJob::new`].
    #[error("cron job id {0:?} already exists")]
    DuplicateJob(String),

    /// Store file present but its on-disk shape doesn't match the
    /// expected schema. The string carries the parser's message.
    #[error("invalid cron store file: {0}")]
    InvalidStore(String),

    /// Schedule construction failed. Surfaces the underlying cause so
    /// CLI / `/cron add` can report which field broke.
    #[error(transparent)]
    Schedule(#[from] ScheduleError),

    /// Disk I/O failed reading or writing the store file.
    #[error("cron store I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed. Distinct from
    /// [`Error::InvalidStore`] because this fires before we even see
    /// a parsed shape.
    #[error("cron store JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
