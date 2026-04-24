//! Slash-command dispatch — the generic router plus its context-parameterized
//! [`Command`] trait.
//!
//! Each channel (and, later, the agent itself) owns a [`Router<Ctx>`] and
//! registers commands that produce a [`CommandOutcome`] when invoked.
//! Routers are pure dispatch tables: they do no I/O and do not know how to
//! "exit a channel" or "display a message" — they only match input against
//! the registry and hand back an outcome for the caller to interpret.
//!
//! Commands that do not depend on a specific channel's state (`Exit`,
//! `Quit`, …) live under [`builtins`] and are generic over `Ctx`, so one
//! instance can register into any router. Channel-specific commands live
//! under that channel's module (for example `crate::cli::commands::Help`)
//! and bind to a concrete context type.

pub mod builtins;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

/// Result of a dispatched command.
///
/// `Exit` asks the channel to shut down; `Feedback` carries a
/// human-readable message the channel should surface to the user
/// however it renders text; `Handled` means the command did its work
/// and nothing further is expected from the caller.
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// Command ran to completion; nothing further for the caller to do.
    Handled,
    /// Channel should shut down.
    ///
    /// The agent-level router has no meaningful interpretation for
    /// this variant and should log-and-ignore if it ever surfaces
    /// there (happens only on a mis-registration).
    Exit,
    /// Message the channel should surface to the user.
    Feedback(String),
}

/// One slash command.
///
/// Implementations are typically zero-sized (`struct Exit;`) when they
/// need no state beyond the dispatch context; bigger commands hold
/// whatever handles they need by value.
#[async_trait]
pub trait Command<Ctx>: Send + Sync {
    /// Command name without the leading slash (for example `"help"`).
    fn name(&self) -> &'static str;

    /// One-line description used by `/help`-style listings.
    fn describe(&self) -> &'static str;

    /// Execute the command. `args` is the trimmed substring that
    /// followed the command name (empty when the user typed only the
    /// name); `ctx` is the dispatch context supplied by the caller.
    async fn execute(&self, args: &str, ctx: &Ctx) -> CommandOutcome;
}

/// A router: a registry of commands indexed by name and a `dispatch`
/// entry point that does exact-match lookup.
///
/// Generic over a caller-supplied context type so the same routing
/// machinery serves channel-local and agent-level command sets with
/// different capability surfaces.
pub struct Router<Ctx> {
    commands: HashMap<String, Arc<dyn Command<Ctx>>>,
}

impl<Ctx> Router<Ctx> {
    /// Construct an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Register a command under the name returned by
    /// [`Command::name`]. A later registration under the same name
    /// overwrites the earlier entry.
    pub fn register(&mut self, cmd: Arc<dyn Command<Ctx>>) {
        let name = cmd.name().to_string();
        self.commands.insert(name, cmd);
    }

    /// Dispatch one command body (the text after the leading `/`).
    ///
    /// Returns `None` when the name does not match any registered
    /// command — the caller decides how to fall back (for example,
    /// channel routers forward unknown commands to the agent router).
    pub async fn dispatch(&self, raw: &str, ctx: &Ctx) -> Option<CommandOutcome> {
        let (name, args) = split_name_args(raw);
        let cmd = self.commands.get(name)?;
        Some(cmd.execute(args, ctx).await)
    }

    /// Sorted `(name, describe)` listing for help text generation.
    #[must_use]
    pub fn list(&self) -> Vec<(&'static str, &'static str)> {
        let mut items: Vec<_> = self
            .commands
            .values()
            .map(|c| (c.name(), c.describe()))
            .collect();
        items.sort_by_key(|(n, _)| *n);
        items
    }
}

impl<Ctx> Default for Router<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a command body into `(name, args)`. `raw` must already have
/// its leading `/` stripped. `args` is the trimmed remainder after the
/// first whitespace, or an empty slice when the user typed only the
/// command name.
fn split_name_args(raw: &str) -> (&str, &str) {
    let raw = raw.trim();
    match raw.find(char::is_whitespace) {
        Some(i) => (&raw[..i], raw[i..].trim_start()),
        None => (raw, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl<Ctx: Send + Sync> Command<Ctx> for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn describe(&self) -> &'static str {
            "reply with the received args"
        }
        async fn execute(&self, args: &str, _ctx: &Ctx) -> CommandOutcome {
            CommandOutcome::Feedback(args.to_string())
        }
    }

    #[tokio::test]
    async fn dispatch_routes_by_name_and_trims_args() {
        let mut router: Router<()> = Router::new();
        router.register(Arc::new(Echo));

        let outcome = router.dispatch("echo   hello world  ", &()).await;
        match outcome {
            Some(CommandOutcome::Feedback(s)) => assert_eq!(s, "hello world"),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_returns_none_on_unknown_name() {
        let router: Router<()> = Router::new();
        assert!(router.dispatch("missing", &()).await.is_none());
    }

    #[test]
    fn split_name_args_handles_empty_and_bare_name() {
        assert_eq!(split_name_args("help"), ("help", ""));
        assert_eq!(split_name_args("help  "), ("help", ""));
        assert_eq!(split_name_args("echo  a  b"), ("echo", "a  b"));
    }
}
