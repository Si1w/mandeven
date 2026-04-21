//! Errors surfaced by the `llm` module.

use thiserror::Error;

/// Errors that can occur while dialing an LLM or processing its response.
#[derive(Debug, Error)]
pub enum Error {
    /// The provider name has no registered implementation.
    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    /// The environment variable named by the provider is not set.
    #[error("missing environment variable for API key: {0}")]
    MissingApiKey(String),

    /// The HTTP request itself failed at transport level.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The API accepted the request but returned a non-success status.
    #[error("API returned error status {status}: {body}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Raw response body.
        body: String,
    },

    /// Response body could not be parsed into the expected shape.
    #[error("failed to deserialize response: {0}")]
    Deserialize(#[from] serde_json::Error),

    /// Request exceeded the configured timeout.
    #[error("request timed out after {secs} seconds")]
    Timeout {
        /// Configured timeout in seconds.
        secs: u64,
    },
}

/// Result alias for the `llm` module.
pub type Result<T> = std::result::Result<T, Error>;
