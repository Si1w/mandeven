//! Errors surfaced by the `hook` module.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Failures from hook configuration loading or hook execution.
#[derive(Debug, Error)]
pub enum Error {
    /// `hooks.json` exists but the read failed.
    #[error("failed to read hooks.json at {}: {source}", path.display())]
    FileRead {
        /// Resolved on-disk path.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `hooks.json` parse failed.
    #[error("failed to parse hooks.json at {}: {source}", path.display())]
    Parse {
        /// Resolved on-disk path.
        path: PathBuf,
        /// JSON parse error.
        #[source]
        source: serde_json::Error,
    },

    /// A `matcher` field in the hook config is not a valid regex.
    #[error("invalid matcher regex {pattern:?}: {source}")]
    InvalidMatcher {
        /// Pattern as written.
        pattern: String,
        /// Underlying regex error.
        #[source]
        source: regex::Error,
    },
}

/// Result alias for the `hook` module.
pub type Result<T> = std::result::Result<T, Error>;
