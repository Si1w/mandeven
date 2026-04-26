//! Errors surfaced by the `prompt` module.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Failures from prompt construction or template I/O.
///
/// Currently scoped to the boot-time `AGENTS.md` read — future
/// dynamic section computations (git status, MCP instructions, …) can
/// extend this enum in place.
#[derive(Debug, Error)]
pub enum Error {
    /// `AGENTS.md` exists at the expected location but the read
    /// failed. Distinguished from "missing file": absent `AGENTS.md`
    /// is a normal state ([`crate::prompt::PromptEngine::load`]
    /// silently treats it as `None`).
    #[error("failed to read AGENTS.md at {}: {source}", path.display())]
    AgentsMdRead {
        /// Resolved on-disk path.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
}

/// Result alias for the `prompt` module.
pub type Result<T> = std::result::Result<T, Error>;
