//! Errors surfaced by the Dream subsystem.

use thiserror::Error;

/// Result alias used across Dream.
pub type Result<T> = std::result::Result<T, Error>;

/// Dream runtime failure.
#[derive(Debug, Error)]
pub enum Error {
    /// Filesystem failure.
    #[error("dream I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization failure.
    #[error("dream JSON failed: {0}")]
    Json(#[from] serde_json::Error),

    /// Schedule parsing failure.
    #[error("dream schedule failed: {0}")]
    Schedule(#[from] crate::timer::ScheduleError),

    /// Session store failure.
    #[error("dream session review failed: {0}")]
    Session(#[from] crate::session::Error),

    /// Memory store failure.
    #[error("dream memory update failed: {0}")]
    Memory(#[from] crate::memory::Error),

    /// LLM completion failure.
    #[error("dream LLM call failed: {0}")]
    Llm(#[from] crate::llm::Error),

    /// The model did not follow the required structured response.
    #[error("dream extraction response was invalid: {0}")]
    InvalidExtraction(String),
}
