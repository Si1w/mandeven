//! Gateway-level slash commands.
//!
//! These commands operate on the gateway's binding state (which
//! storage [`SessionID`] is currently bound to a given inbound
//! identity) and on the session store. They run after channel-local commands miss
//! and before falling through to the agent router; channels reach
//! them by forwarding [`crate::bus::InboundPayload::Command`] over
//! the bus.
//!
//! Currently handled:
//!
//! - `/new` — bind the inbound identity to a fresh session id
//! - `/list` — list this identity's known sessions, snapshot for
//!   subsequent `/load <n>`
//! - `/load <n>` — bind the identity to the n-th session in the
//!   most recent `/list` snapshot

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::bus::{ChannelID, SessionID};
use crate::command::CommandOutcome;
use crate::gateway::SessionKey;
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
    /// outbound replies.
    pub channel: ChannelID,
    /// Full inbound identity key used for session binding.
    pub session_key: SessionKey,
    /// Bindings from identity key to the currently bound storage
    /// session. `/new` and `/load` mutate the entry for `session_key`.
    pub active_sessions: Arc<Mutex<HashMap<SessionKey, SessionID>>>,
    /// Per-identity snapshot of the most recent `/list` output, used
    /// by `/load <n>` to resolve a numeric index back to a concrete
    /// `SessionID` without re-listing.
    pub last_listed: Arc<Mutex<HashMap<SessionKey, Vec<SessionID>>>>,
    /// Session store handle.
    pub sessions: Arc<session::Manager>,
}

impl GatewayCommandCtx {
    async fn set_active(&self, id: SessionID) {
        self.active_sessions
            .lock()
            .await
            .insert(self.session_key.clone(), id);
    }

    async fn set_last_listed(&self, ids: Vec<SessionID>) {
        self.last_listed
            .lock()
            .await
            .insert(self.session_key.clone(), ids);
    }

    async fn lookup_listed(&self, idx: usize) -> Option<SessionID> {
        let map = self.last_listed.lock().await;
        map.get(&self.session_key).and_then(|v| v.get(idx).cloned())
    }

    pub(crate) async fn new_session(&self) -> CommandOutcome {
        let id = SessionID::new();
        self.set_active(id).await;
        CommandOutcome::Completed
    }

    pub(crate) async fn list_sessions(&self) -> CommandOutcome {
        let ids = match self.sessions.list().await {
            Ok(v) => v,
            Err(err) => return CommandOutcome::Feedback(format!("failed to list sessions: {err}")),
        };

        let mut entries: Vec<(SessionID, String, DateTime<Utc>)> = Vec::new();
        for id in ids {
            // Skip both "vanished between list and read" (Ok(None))
            // and corrupt files (Err) — neither should block /list
            // from showing the rest. Corruption surfaces loudly via
            // the load path instead.
            if let Ok(Some(meta)) = self.sessions.metadata(&id).await
                && self.session_key.matches_metadata(&meta)
            {
                entries.push((id, meta.title, meta.updated_at));
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.2));

        if entries.is_empty() {
            return CommandOutcome::Feedback("no sessions yet".to_string());
        }

        let snapshot: Vec<SessionID> = entries.iter().map(|(id, _, _)| id.clone()).collect();
        self.set_last_listed(snapshot).await;

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

    pub(crate) async fn load_session(&self, n: usize) -> CommandOutcome {
        if n == 0 {
            return CommandOutcome::Feedback("indices start at 1".to_string());
        }
        let Some(target) = self.lookup_listed(n - 1).await else {
            return CommandOutcome::Feedback(format!(
                "no session at index {n}; run /list first or pick a smaller number"
            ));
        };

        // Confirm the file still exists before swapping bindings —
        // a concurrent process or manual `rm` between /list and
        // /load shouldn't leave the channel pointing at a hole.
        // Success returns `Completed`; the visible effect is the
        // `SessionSwitched` notice the gateway emits when it sees
        // the binding changed, which drives the transcript rebuild.
        match self.sessions.metadata(&target).await {
            Ok(Some(_)) => {
                self.set_active(target).await;
                CommandOutcome::Completed
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
