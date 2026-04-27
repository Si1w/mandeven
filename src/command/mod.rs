//! Slash-command parsing and shared command outcomes.
//!
//! Parsing is centralized in [`slash`], which wraps clap and produces typed
//! command enums. CLI, gateway, and agent layers then decide which parsed
//! commands they own.

pub mod slash;

/// Result of running a slash command handler.
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// Command ran to completion; nothing further for the caller to do.
    Completed,
    /// Channel should shut down.
    Exit,
    /// Message the channel should surface to the user.
    Feedback(String),
}
