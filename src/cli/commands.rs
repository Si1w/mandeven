//! CLI-specific slash commands.
//!
//! These commands bind to [`CliCommandCtx`] because they mutate the
//! channel's UI state (overlay, transcript) rather than producing a
//! pure textual reply. Cross-channel commands (`/exit`, `/quit`) live
//! in [`crate::command::builtins`] and are registered alongside these
//! in [`crate::cli::CliChannel::new`].

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::command::{Command, CommandOutcome};

use super::{CliState, Overlay};

/// Execution context handed to CLI-local commands.
///
/// A lightweight bundle of the handles a command might need to mutate
/// UI state. Cheap to clone (two `Arc` clones per dispatch) — the
/// channel constructs one per call rather than caching, to avoid
/// extra lifetime coupling between command objects and the channel.
pub struct CliCommandCtx {
    /// Shared UI state; commands lock this to push transcript lines,
    /// toggle overlays, and so on.
    pub state: Arc<Mutex<CliState>>,
    /// Render notifier; commands call `notify_one` after mutating
    /// state so the pending frame reflects their changes.
    pub redraw: Arc<Notify>,
}

/// `/help` — open the help overlay.
pub struct Help;

#[async_trait]
impl Command<CliCommandCtx> for Help {
    fn name(&self) -> &'static str {
        "help"
    }
    fn describe(&self) -> &'static str {
        "show the help overlay"
    }
    async fn execute(&self, _args: &str, ctx: &CliCommandCtx) -> CommandOutcome {
        ctx.state.lock().unwrap().overlay = Some(Overlay::Help);
        ctx.redraw.notify_one();
        CommandOutcome::Handled
    }
}

/// `/skills` — open the skills overlay listing every loaded SKILL.md
/// by name + description.
///
/// The overlay reads the same [`crate::skill::SkillIndex`] the
/// `/<skill-name>` slash-command fallback uses, so the list shown
/// here is exactly the set of names that will resolve to a skill
/// invocation.
pub struct Skills;

#[async_trait]
impl Command<CliCommandCtx> for Skills {
    fn name(&self) -> &'static str {
        "skills"
    }
    fn describe(&self) -> &'static str {
        "show the skills overlay"
    }
    async fn execute(&self, _args: &str, ctx: &CliCommandCtx) -> CommandOutcome {
        ctx.state.lock().unwrap().overlay = Some(Overlay::Skills);
        ctx.redraw.notify_one();
        CommandOutcome::Handled
    }
}
