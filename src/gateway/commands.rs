//! Gateway-level slash commands.
//!
//! These commands operate on the gateway's binding state (which
//! storage [`SessionID`] is currently bound to a given channel) and
//! on the session store. They run after channel-local commands miss
//! and before falling through to the agent router; channels reach
//! them by forwarding [`crate::bus::InboundPayload::Command`] over
//! the bus.
//!
//! Currently registered:
//!
//! - `/new` — bind the channel to a fresh session id
//! - `/list` — list this channel's known sessions, snapshot for
//!   subsequent `/load <n>`
//! - `/load <n>` — bind the channel to the n-th session in the
//!   most recent `/list` snapshot

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::bus::{ChannelID, SessionID};
use crate::command::{Command, CommandOutcome};
use crate::session;

/// How session timestamps are formatted in `/list` output.
const LIST_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M";

/// Truncation cap for session titles in `/list` output. Keeps each
/// row narrow enough for an 80-column terminal. UTF-8 safe via
/// `chars().take()`.
const LIST_TITLE_MAX_CHARS: usize = 60;

/// Execution context for gateway-level commands. Holds shared
/// handles to the gateway state the commands mutate or read.
///
/// Constructed fresh per dispatch by [`crate::gateway::Gateway`]; an
/// `Arc` clone of each field is cheap so this is not a hot path.
pub struct GatewayCommandCtx {
    /// Channel originating this command — used as the key into
    /// [`Self::active_sessions`] and [`Self::last_listed`].
    pub channel: ChannelID,
    /// Bindings from a channel to the currently bound storage
    /// session. `/new` and `/load` mutate the entry for `channel`.
    pub active_sessions: Arc<Mutex<HashMap<ChannelID, SessionID>>>,
    /// Per-channel snapshot of the most recent `/list` output, used
    /// by `/load <n>` to resolve a numeric index back to a concrete
    /// `SessionID` without re-listing.
    pub last_listed: Arc<Mutex<HashMap<ChannelID, Vec<SessionID>>>>,
    /// Session store handle.
    pub sessions: Arc<session::Manager>,
}

impl GatewayCommandCtx {
    async fn set_active(&self, id: SessionID) {
        self.active_sessions
            .lock()
            .await
            .insert(self.channel.clone(), id);
    }

    async fn set_last_listed(&self, ids: Vec<SessionID>) {
        self.last_listed
            .lock()
            .await
            .insert(self.channel.clone(), ids);
    }

    async fn lookup_listed(&self, idx: usize) -> Option<SessionID> {
        let map = self.last_listed.lock().await;
        map.get(&self.channel).and_then(|v| v.get(idx).cloned())
    }
}

/// `/new` — bind the channel to a fresh `SessionID`. The session
/// file is **not** pre-created; the agent's `ensure_session` will
/// create it (with a real title generated from the first user
/// message) on the next user input. Returns
/// [`CommandOutcome::Handled`] so the *only* channel-visible effect
/// is the [`crate::bus::OutboundPayload::SessionSwitched`] notice the
/// gateway emits when it detects the binding change — the CLI clears
/// its transcript on that, and an extra "started a new session" line
/// would just be noise.
pub struct New;

#[async_trait]
impl Command<GatewayCommandCtx> for New {
    fn name(&self) -> &'static str {
        "new"
    }
    fn describe(&self) -> &'static str {
        "start a fresh session"
    }
    async fn execute(&self, _args: &str, ctx: &GatewayCommandCtx) -> CommandOutcome {
        let id = SessionID::new();
        ctx.set_active(id).await;
        CommandOutcome::Handled
    }
}

/// `/list` — list the channel's known sessions sorted newest-first
/// by `updated_at`. Stores a parallel `Vec<SessionID>` snapshot in
/// [`GatewayCommandCtx::last_listed`] so a subsequent `/load <n>`
/// can resolve the numeric index without re-walking the store.
pub struct List;

#[async_trait]
impl Command<GatewayCommandCtx> for List {
    fn name(&self) -> &'static str {
        "list"
    }
    fn describe(&self) -> &'static str {
        "list sessions on this channel; pair with /load <n>"
    }
    async fn execute(&self, _args: &str, ctx: &GatewayCommandCtx) -> CommandOutcome {
        let ids = match ctx.sessions.list().await {
            Ok(v) => v,
            Err(err) => return CommandOutcome::Feedback(format!("failed to list sessions: {err}")),
        };

        let mut entries: Vec<(SessionID, String, DateTime<Utc>)> = Vec::new();
        for id in ids {
            // Skip both "vanished between list and read" (Ok(None))
            // and corrupt files (Err) — neither should block /list
            // from showing the rest. Corruption surfaces loudly via
            // the load path instead.
            if let Ok(Some(meta)) = ctx.sessions.metadata(&id).await
                && meta.channel == ctx.channel
            {
                entries.push((id, meta.title, meta.updated_at));
            }
        }
        entries.sort_by(|a, b| b.2.cmp(&a.2));

        if entries.is_empty() {
            return CommandOutcome::Feedback("no sessions yet".to_string());
        }

        let snapshot: Vec<SessionID> = entries.iter().map(|(id, _, _)| id.clone()).collect();
        ctx.set_last_listed(snapshot).await;

        let mut out = String::new();
        out.push_str("Sessions (newest first):\n");
        for (i, (_, title, updated)) in entries.iter().enumerate() {
            let title_trim = truncate_chars(title, LIST_TITLE_MAX_CHARS);
            let stamp = updated.format(LIST_TIMESTAMP_FORMAT);
            // `writeln!` on a `String` is infallible.
            let _ = writeln!(out, "  [{}] {stamp}  {title_trim}", i + 1);
        }
        out.push_str("Type /load <n> to switch.");
        CommandOutcome::Feedback(out)
    }
}

/// `/load <n>` — bind the channel to the n-th session from the most
/// recent `/list` snapshot. 1-based indexing because that is what
/// `/list` displays. Empty snapshot → instructive feedback. Out-of-
/// range index → instructive feedback. Successful switch returns
/// `Handled` so the gateway emits `SessionSwitched` and a notice
/// from a single dispatch site rather than mixing them in here.
pub struct Load;

#[async_trait]
impl Command<GatewayCommandCtx> for Load {
    fn name(&self) -> &'static str {
        "load"
    }
    fn describe(&self) -> &'static str {
        "switch to a session from the latest /list (use /load <n>)"
    }
    async fn execute(&self, args: &str, ctx: &GatewayCommandCtx) -> CommandOutcome {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandOutcome::Feedback(
                "usage: /load <n>; run /list first to see indices".to_string(),
            );
        }
        let Ok(n) = trimmed.parse::<usize>() else {
            return CommandOutcome::Feedback(format!("expected a number, got '{trimmed}'"));
        };
        if n == 0 {
            return CommandOutcome::Feedback("indices start at 1".to_string());
        }
        let Some(target) = ctx.lookup_listed(n - 1).await else {
            return CommandOutcome::Feedback(format!(
                "no session at index {n}; run /list first or pick a smaller number"
            ));
        };

        // Confirm the file still exists before swapping bindings —
        // a concurrent process or manual `rm` between /list and
        // /load shouldn't leave the channel pointing at a hole.
        // Success returns `Handled`; the visible effect is the
        // `SessionSwitched` notice the gateway emits when it sees
        // the binding changed, which drives the transcript rebuild.
        match ctx.sessions.metadata(&target).await {
            Ok(Some(_)) => {
                ctx.set_active(target).await;
                CommandOutcome::Handled
            }
            Ok(None) => CommandOutcome::Feedback(format!(
                "session {n} no longer exists; run /list to refresh"
            )),
            Err(err) => CommandOutcome::Feedback(format!("failed to load session: {err}")),
        }
    }
}

/// Truncate a string to at most `max` Unicode scalar values,
/// appending an ellipsis when truncation actually happened.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut byte_end = s.len();
    for (count, (i, _)) in s.char_indices().enumerate() {
        if count == max {
            byte_end = i;
            break;
        }
    }
    if byte_end < s.len() {
        format!("{}…", &s[..byte_end])
    } else {
        s.to_string()
    }
}
