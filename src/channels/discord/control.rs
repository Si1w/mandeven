//! Runtime control handle for the Discord adapter.
//!
//! Owned by `main.rs`, cloned into the agent's slash-command context
//! ([`crate::agent::command::AgentCommandCtx`]) so `/discord ...`
//! mutates the same allow list / connection state that the bridge
//! consults on every inbound DM.
//!
//! Three concerns share this one handle:
//!
//! 1. **Allow list** — `allow|deny|list` mutate the shared
//!    [`AllowList`] (Arc-backed) and persist to the JSON sidecar.
//! 2. **Connection lifecycle** — `enable|disable` flip a watch
//!    channel that the supervisor loop in
//!    [`super::DiscordChannel::start`] subscribes to.
//! 3. **Token resolution** — `enable` re-reads the configured env
//!    var so token rotation works without a process restart.
//!
//! Cloning the handle is cheap; every clone shares the same
//! underlying state.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, watch};

use crate::channels::common::AllowList;

use super::store;

const SINGLE_USER_MS0_ERROR: &str =
    "discord MS0 supports one allowed user; deny the existing user before allowing another";

/// Snapshot of the adapter's runtime state — what `/discord` (no
/// args) renders.
#[derive(Clone, Debug)]
pub struct DiscordStatus {
    /// Whether the gateway connection is currently desired.
    /// `true` once `enable()` succeeded; `false` after `disable()`.
    /// The actual connection may lag by a few ms during transitions.
    pub active: bool,
    /// Number of user ids currently in the allow list.
    pub allowed_count: usize,
}

/// Handle for runtime allowlist + connection mutations.
#[derive(Clone)]
pub struct DiscordControl {
    /// Cheap-clone allow list shared with the bridge's event handler;
    /// every mutation here is immediately visible to inbound filtering.
    allowed: AllowList<u64>,
    /// On-disk sidecar path. Wrapped in `Arc` so clones share one
    /// allocation; the `PathBuf` itself is read-only after construction.
    store_path: Arc<PathBuf>,
    /// Active-state writer. The supervisor loop subscribes to the
    /// matching receiver and (de)connects accordingly.
    active: Arc<watch::Sender<bool>>,
    /// Token storage, written by [`Self::enable`] before flipping
    /// `active = true` so the supervisor finds a token when it wakes.
    token: Arc<Mutex<Option<String>>>,
    /// Name of the env var holding the bot token. Re-read on each
    /// [`Self::enable`] call.
    token_env: String,
}

impl DiscordControl {
    /// Construct a control handle.
    ///
    /// `allowed`, `active`, and `token` must be the same handles
    /// stored on the paired [`super::state::DiscordState`] — see
    /// [`super::DiscordChannel::build`] for the canonical wiring.
    #[must_use]
    pub fn new(
        allowed: AllowList<u64>,
        store_path: PathBuf,
        active: Arc<watch::Sender<bool>>,
        token: Arc<Mutex<Option<String>>>,
        token_env: String,
    ) -> Self {
        Self {
            allowed,
            store_path: Arc::new(store_path),
            active,
            token,
            token_env,
        }
    }

    /// Add `user_id` to the allow list and persist if it changed.
    ///
    /// Returns `Ok(true)` when newly added, `Ok(false)` when already
    /// present.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] of kind [`io::ErrorKind::InvalidInput`]
    /// when another user is already allowed. Returns [`io::Error`]
    /// when persistence fails. The in-memory
    /// allow list is **not** rolled back on disk failure — the
    /// runtime state already reflects the addition; the next
    /// successful save catches up.
    pub async fn allow(&self, user_id: u64) -> io::Result<bool> {
        let current = self.allowed.snapshot();
        if current.contains(&user_id) {
            return Ok(false);
        }
        if !current.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                SINGLE_USER_MS0_ERROR,
            ));
        }

        let added = self.allowed.insert(user_id);
        if added {
            self.persist().await?;
        }
        Ok(added)
    }

    /// Remove `user_id`. Returns `Ok(true)` when an entry was
    /// removed, `Ok(false)` when the id was absent.
    ///
    /// # Errors
    ///
    /// See [`Self::allow`].
    pub async fn deny(&self, user_id: u64) -> io::Result<bool> {
        let removed = self.allowed.remove(&user_id);
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    /// Snapshot the current allow list, sorted ascending for stable
    /// display in `/discord list`.
    #[must_use]
    pub fn list(&self) -> Vec<u64> {
        let mut v = self.allowed.snapshot();
        v.sort_unstable();
        v
    }

    /// Open the gateway connection.
    ///
    /// Idempotent — calling twice is a no-op the second time.
    /// Re-reads the bot token from the configured environment
    /// variable so token rotation does not require a restart.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] of kind [`io::ErrorKind::NotFound`] when
    /// the env var is unset or contains invalid UTF-8. Connection
    /// failures (bad token, network) surface later in the
    /// supervisor loop's logs, not here — `enable()` only stages the
    /// request.
    pub async fn enable(&self) -> io::Result<bool> {
        if *self.active.borrow() {
            return Ok(false);
        }
        let token = std::env::var(&self.token_env).map_err(|err| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("${} not set: {err}", self.token_env),
            )
        })?;
        *self.token.lock().await = Some(token);
        let _ = self.active.send(true);
        Ok(true)
    }

    /// Close the gateway connection. Idempotent.
    ///
    /// Returns `true` when this call flipped the flag from active to
    /// inactive, `false` when the connection was already closed.
    #[must_use]
    pub fn disable(&self) -> bool {
        if !*self.active.borrow() {
            return false;
        }
        let _ = self.active.send(false);
        true
    }

    /// Subscribe to active-state transitions.
    ///
    /// Surfaces the underlying `tokio::sync::watch` receiver so UIs
    /// (currently the TUI top bar) can repaint when `/discord`
    /// flips the connection. Each call returns a fresh receiver
    /// pre-seeded with the current value.
    #[must_use]
    pub fn subscribe_active(&self) -> watch::Receiver<bool> {
        self.active.subscribe()
    }

    /// Snapshot the current state.
    #[must_use]
    pub fn status(&self) -> DiscordStatus {
        DiscordStatus {
            active: *self.active.borrow(),
            allowed_count: self.allowed.snapshot().len(),
        }
    }

    async fn persist(&self) -> io::Result<()> {
        let ids: std::collections::HashSet<u64> = self.allowed.snapshot().into_iter().collect();
        store::save(&self.store_path, &ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::DiscordControl;
    use crate::channels::common::AllowList;
    use std::sync::Arc;
    use tokio::sync::{Mutex, watch};

    fn tmp_path(label: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mandeven-discord-control-{}-{}.json",
            label,
            uuid::Uuid::now_v7()
        ));
        p
    }

    fn fixture(label: &str, allowed: AllowList<u64>) -> DiscordControl {
        let (tx, _rx) = watch::channel(false);
        DiscordControl::new(
            allowed,
            tmp_path(label),
            Arc::new(tx),
            Arc::new(Mutex::new(None)),
            "DISCORD_BOT_TOKEN_TEST".to_string(),
        )
    }

    #[tokio::test]
    async fn allow_persists_new_ids() {
        let ctl = fixture("allow", AllowList::with_initial(std::iter::empty::<u64>()));
        assert!(ctl.allow(123).await.expect("allow"));
        assert!(!ctl.allow(123).await.expect("allow")); // idempotent
    }

    #[tokio::test]
    async fn deny_persists_removal() {
        let ctl = fixture("deny", AllowList::with_initial([42u64, 7]));
        assert!(ctl.deny(42).await.expect("deny"));
        assert!(!ctl.deny(42).await.expect("deny")); // idempotent
    }

    #[test]
    fn list_returns_sorted_snapshot() {
        let ctl = fixture("list", AllowList::with_initial([3u64, 1, 2]));
        assert_eq!(ctl.list(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn allow_rejects_second_user_in_ms0() {
        let ctl = fixture("single", AllowList::with_initial([1u64]));
        let err = ctl.allow(2).await.expect_err("second user should fail");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(ctl.list(), vec![1]);
    }

    #[tokio::test]
    async fn enable_without_env_var_errors() {
        let ctl = fixture("env", AllowList::with_initial(std::iter::empty::<u64>()));
        // Variable name is unique to this test so the env stays clean.
        let err = ctl.enable().await.expect_err("missing env should error");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!ctl.status().active);
    }

    #[tokio::test]
    async fn disable_is_idempotent_when_already_off() {
        let ctl = fixture("dis", AllowList::with_initial(std::iter::empty::<u64>()));
        assert!(!ctl.disable());
        assert!(!ctl.status().active);
    }
}
