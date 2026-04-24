//! Agent-level slash commands.
//!
//! These commands are dispatched by the agent loop when a channel
//! forwards an unknown command via [`crate::bus::InboundPayload::Command`].
//! They operate on agent-global concepts (sessions, tasks, model
//! state) and reply through the outbound bus rather than by mutating
//! any specific channel's UI.
//!
//! Context design: [`AgentCommandCtx`] starts with only the routing
//! handles (channel + session) every command needs to address its
//! reply. Richer handles (session manager, task registry, …) are
//! added here as concrete commands require them — we deliberately
//! avoid a god-ctx that exposes every agent field up-front.

use async_trait::async_trait;

use crate::bus::{ChannelID, SessionID};
use crate::command::{Command, CommandOutcome};

/// Execution context for agent-level commands.
///
/// Kept intentionally small; commands that need more (session
/// history, active task list, etc.) grow this struct with additional
/// handles when they land. Named with `CommandCtx` rather than bare
/// `Context` to avoid colliding with the conversation-history sense
/// of "context" elsewhere in the codebase (sessions, LLM context
/// windows, …).
pub struct AgentCommandCtx {
    /// Channel that originated the command; used by the agent loop
    /// to route the outbound reply back.
    pub channel: ChannelID,
    /// Session the command runs within.
    pub session: SessionID,
}

/// `/ping` — reply `pong`. Minimal command used to verify the
/// channel → agent → outbound round-trip is wired correctly.
pub struct Ping;

#[async_trait]
impl Command<AgentCommandCtx> for Ping {
    fn name(&self) -> &'static str {
        "ping"
    }
    fn describe(&self) -> &'static str {
        "reply with 'pong' — round-trip liveness check"
    }
    async fn execute(&self, _args: &str, _ctx: &AgentCommandCtx) -> CommandOutcome {
        CommandOutcome::Feedback("pong".to_string())
    }
}
