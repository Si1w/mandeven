//! Errors surfaced by the `MEMORY.md` subsystem.

use thiserror::Error;

/// Result alias used across memory loading and validation.
pub type Result<T> = std::result::Result<T, Error>;

/// Operations a memory caller can fail at.
#[derive(Debug, Error)]
pub enum Error {
    /// Content was rejected before it could be written or injected.
    #[error("unsafe memory content: {0}")]
    UnsafeContent(String),

    /// Disk I/O failed reading or writing `MEMORY.md`.
    #[error("memory I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
