//! Errors surfaced by the run history module.

use thiserror::Error;

/// Result alias used across run history operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Run history read/write failures.
#[derive(Debug, Error)]
pub enum Error {
    /// Disk I/O failed reading or writing a run log.
    #[error("run history I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("run history JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
