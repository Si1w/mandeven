//! Agent-level slash commands.
//!
//! Reserved for commands that operate on **agent-internal state** —
//! cancelling an in-flight iteration (`/stop`), reporting agent
//! status (`/status`), forcing a context compact, etc. Routing /
//! session-level commands (`/new`, `/list`, `/load`) live one layer
//! up in the gateway, where the binding table and session store
//! handles already sit.
//!
//! This module is intentionally empty in the current build. The
//! [`AgentCommandCtx`] type and the agent's `Router<AgentCommandCtx>`
//! field stay wired so the first real agent-level command can drop
//! straight in without re-introducing the plumbing.

use crate::bus::{ChannelID, SessionID};

/// Execution context for agent-level commands.
///
/// Kept intentionally small; commands that need richer handles
/// (active task list, current iteration, etc.) extend this struct
/// when they land. Named with `CommandCtx` rather than bare
/// `Context` to avoid colliding with the conversation-history
/// sense of "context" elsewhere in the codebase.
pub struct AgentCommandCtx {
    /// Channel that originated the command; used by the agent loop
    /// to address the outbound reply.
    pub channel: ChannelID,
    /// Session the command runs within.
    pub session: SessionID,
}
