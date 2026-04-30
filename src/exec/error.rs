//! Errors surfaced by the execution history module.

use thiserror::Error;

/// Result alias used across execution history operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Execution history read/write failures.
#[derive(Debug, Error)]
pub enum Error {
    /// Disk I/O failed reading or writing an execution log.
    #[error("execution history I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("execution history JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
