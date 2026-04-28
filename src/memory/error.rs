//! Errors surfaced by the memory module.

use thiserror::Error;

/// Result alias used across memory storage and mutation.
pub type Result<T> = std::result::Result<T, Error>;

/// Operations a memory caller can fail at.
#[derive(Debug, Error)]
pub enum Error {
    /// Memory lookup failed.
    #[error("memory {0} not found")]
    MemoryNotFound(String),

    /// Store file present but its on-disk shape is unsupported.
    #[error("invalid memory store file: {0}")]
    InvalidStore(String),

    /// Caller supplied an invalid field.
    #[error("invalid memory field {field}: {message}")]
    InvalidField {
        /// Field name.
        field: &'static str,
        /// Validation message.
        message: String,
    },

    /// Content was rejected before it could be injected into future prompts.
    #[error("unsafe memory content: {0}")]
    UnsafeContent(String),

    /// Disk I/O failed reading or writing the store file.
    #[error("memory store I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("memory store JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
