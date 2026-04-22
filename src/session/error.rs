//! Errors surfaced by the `session` module.

use thiserror::Error;

use crate::bus::SessionID;

/// Errors that can occur while reading or writing a session file.
#[derive(Debug, Error)]
pub enum Error {
    /// Filesystem-level failure (open, read, write, rename).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization or deserialization failure on a metadata or
    /// message line.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// `append` was called on a session whose file does not exist.
    /// Call [`crate::session::Manager::create`] first.
    #[error("session not found: {0:?}")]
    NotFound(SessionID),

    /// The session file exists but does not match the expected
    /// `metadata line + message lines` layout.
    #[error("invalid session file format: {0}")]
    InvalidFormat(String),
}

/// Result alias for the `session` module.
pub type Result<T> = std::result::Result<T, Error>;
