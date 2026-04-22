//! Errors surfaced by the `tools` module.

use thiserror::Error;

/// Errors that can occur while invoking a tool.
///
/// All variants are recoverable: [`crate::tools::Registry::dispatch`]
/// catches them and feeds the error back to the model as a
/// [`crate::llm::Message::Tool`] payload, rather than propagating.
/// The enum remains public so individual [`crate::tools::BaseTool`]
/// implementations can construct [`Self::Execution`] directly.
#[derive(Debug, Error)]
pub enum Error {
    /// No tool is registered under the given name.
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    /// The model-emitted `arguments` string did not parse as JSON.
    #[error("invalid arguments for tool {tool}: {source}")]
    InvalidArguments {
        /// Tool whose arguments failed to parse.
        tool: String,
        /// Underlying JSON parse error.
        #[source]
        source: serde_json::Error,
    },

    /// The tool's own `call` implementation reported a runtime failure.
    #[error("tool {tool} failed: {message}")]
    Execution {
        /// Tool that failed.
        tool: String,
        /// Free-form message supplied by the tool implementation.
        message: String,
    },
}

/// Result alias for the `tools` module.
pub type Result<T> = std::result::Result<T, Error>;
