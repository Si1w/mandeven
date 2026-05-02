//! Errors surfaced by the timer module.

use thiserror::Error;

/// Result alias used across timer storage and mutation.
pub type Result<T> = std::result::Result<T, Error>;

/// Operations a timer caller can fail at.
#[derive(Debug, Error)]
pub enum Error {
    /// Timer lookup failed.
    #[error("timer #{0} not found")]
    TimerNotFound(String),

    /// Referenced task lookup failed.
    #[error("task #{0} not found")]
    TaskNotFound(String),

    /// Store file present but its on-disk shape does not match the
    /// schema this build supports.
    #[error("invalid timer store file: {0}")]
    InvalidStore(String),

    /// Caller supplied an empty field that must contain text.
    #[error("invalid timer field {field}: {message}")]
    InvalidField {
        /// Field name.
        field: &'static str,
        /// Validation message.
        message: String,
    },

    /// Task store operation failed while validating task references.
    #[error("task store error while validating timer: {0}")]
    TaskStore(#[from] crate::task::Error),

    /// Schedule validation failed.
    #[error("invalid timer schedule: {0}")]
    Schedule(#[from] crate::timer::ScheduleError),

    /// Disk I/O failed reading or writing the store file.
    #[error("timer store I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// TOML front matter deserialization failed.
    #[error("timer store TOML decode error: {0}")]
    TomlDecode(#[from] toml::de::Error),

    /// TOML front matter serialization failed.
    #[error("timer store TOML encode error: {0}")]
    TomlEncode(#[from] toml::ser::Error),

    /// JSON serialization or deserialization failed for inline
    /// front matter values such as schedules.
    #[error("timer store JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// YAML front matter serialization or deserialization failed.
    #[error("timer store YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
