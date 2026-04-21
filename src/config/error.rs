//! Errors surfaced by the `config` module.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Errors that can occur while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be opened or read.
    #[error("failed to read config file {}: {source}", path.display())]
    Read {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying IO error.
        source: io::Error,
    },

    /// The file contents are not valid TOML or do not match the schema.
    #[error("failed to parse TOML: {0}")]
    Parse(#[from] toml::de::Error),

    /// `./mandeven.toml` was not found in the current working directory.
    #[error("./mandeven.toml not found in current working directory")]
    NotFound,

    /// A field parsed successfully but carries a semantically invalid value.
    #[error("invalid value for {field}: {reason}")]
    Invalid {
        /// Dotted path of the offending field (for example `"llm.default"`).
        field: &'static str,
        /// Human-readable explanation of why the value is rejected.
        reason: String,
    },
}

/// Result alias for the `config` module.
pub type Result<T> = std::result::Result<T, ConfigError>;
