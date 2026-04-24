//! Context-agnostic built-in commands.
//!
//! Each type here implements [`Command<Ctx>`](super::Command) for any
//! `Ctx: Send + Sync`, so the same instance can register into a
//! channel-local router or the agent router without recompiling per
//! context. Anything that needs channel-specific state belongs in
//! that channel's own module instead.

use async_trait::async_trait;

use super::{Command, CommandOutcome};

/// `/exit` — ask the channel to shut down.
///
/// Returns [`CommandOutcome::Exit`]. The agent-level router should
/// never see this outcome (it has no shutdown semantics), so
/// registering `Exit` there is a mistake the agent is free to warn
/// about.
pub struct Exit;

#[async_trait]
impl<Ctx: Send + Sync> Command<Ctx> for Exit {
    fn name(&self) -> &'static str {
        "exit"
    }
    fn describe(&self) -> &'static str {
        "exit this channel"
    }
    async fn execute(&self, _args: &str, _ctx: &Ctx) -> CommandOutcome {
        CommandOutcome::Exit
    }
}

/// `/quit` — alias of [`Exit`].
///
/// Registered as a separate command because both names are common
/// enough that users will reach for either; behavior is identical.
pub struct Quit;

#[async_trait]
impl<Ctx: Send + Sync> Command<Ctx> for Quit {
    fn name(&self) -> &'static str {
        "quit"
    }
    fn describe(&self) -> &'static str {
        "exit this channel (alias of /exit)"
    }
    async fn execute(&self, _args: &str, _ctx: &Ctx) -> CommandOutcome {
        CommandOutcome::Exit
    }
}
