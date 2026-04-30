//! Errors surfaced by the task module.

use thiserror::Error;

/// Result alias used across task storage and mutation.
pub type Result<T> = std::result::Result<T, Error>;

/// Operations a task caller can fail at.
#[derive(Debug, Error)]
pub enum Error {
    /// Task lookup failed.
    #[error("task #{0} not found")]
    TaskNotFound(String),

    /// A mutation referenced the task itself as a dependency.
    #[error("task #{0} cannot depend on itself")]
    SelfDependency(String),

    /// Store file present but its on-disk shape does not match the
    /// schema this build supports.
    #[error("invalid task store file: {0}")]
    InvalidStore(String),

    /// Caller supplied an empty field that must contain text.
    #[error("invalid task field {field}: {message}")]
    InvalidField {
        /// Field name.
        field: &'static str,
        /// Validation message.
        message: String,
    },

    /// Disk I/O failed reading or writing the store file.
    #[error("task store I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed. Kept for reading
    /// legacy `tasks.json` stores during migration.
    #[error("task store JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML front matter deserialization failed.
    #[error("task store TOML decode error: {0}")]
    TomlDecode(#[from] toml::de::Error),

    /// TOML front matter serialization failed.
    #[error("task store TOML encode error: {0}")]
    TomlEncode(#[from] toml::ser::Error),

    /// YAML front matter serialization or deserialization failed.
    #[error("task store YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
