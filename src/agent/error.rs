//! Errors surfaced by the `agent` module.

use thiserror::Error;

use crate::{bus, llm, memory, session};

/// Errors that can occur while the agent is running.
#[derive(Debug, Error)]
pub enum Error {
    /// Propagated from an LLM call.
    #[error("LLM error: {0}")]
    Llm(#[from] llm::Error),

    /// Propagated from the session store.
    #[error("session error: {0}")]
    Session(#[from] session::Error),

    /// Propagated from publishing on the bus.
    #[error("bus error: {0}")]
    Bus(#[from] bus::Error),

    /// Propagated from loading or validating `MEMORY.md`.
    #[error("memory error: {0}")]
    Memory(#[from] memory::Error),

    /// The provider named in config is not registered in
    /// [`crate::llm::providers`].
    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    /// The `llm.default` string is not of the form `"provider/model"`.
    #[error("malformed profile id: {0} (expected \"provider/model\")")]
    MalformedProfileId(String),

    /// `llm.default` references a (provider, model) pair that has no
    /// matching entry in the config's profile catalog.
    #[error("profile not found: {provider}/{model}")]
    ProfileNotFound {
        /// Provider segment of the missing profile id.
        provider: String,
        /// Model segment of the missing profile id.
        model: String,
    },

    /// The inner LLM↔tool loop ran past its configured iteration cap.
    #[error("max iterations exceeded: {0}")]
    MaxIterationsExceeded(u8),

    /// The provider violated the streaming tool-call protocol (for
    /// example, closed the stream with a tool-call index that never
    /// carried an `id` or `name`).
    #[error("malformed stream: {0}")]
    MalformedStream(String),
}

/// Result alias for the `agent` module.
pub type Result<T> = std::result::Result<T, Error>;
