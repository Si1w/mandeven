//! Shared mutable state between [`super::DiscordChannel`] and the
//! serenity event handler in [`super::bridge`].
//!
//! Wrapped in an `Arc` and handed to both sides so the handler can
//! record the bot's own user id (READY event) and the DM channel id
//! (every inbound message), while the channel side reads them when
//! routing outbound payloads back to Discord.
//!
//! The allow list, the resolved bot token, and the active flag all
//! live here too. The supervisor loop in
//! [`super::DiscordChannel::start`] subscribes to the active flag's
//! `watch` channel; [`super::DiscordControl`] holds clones of the
//! same handles and writes them when `/discord` toggles the adapter.
//! Every clone observes mutations through the underlying `Arc`s —
//! no notification mechanism needed beyond the watch itself.

use std::collections::HashMap;
use std::sync::Arc;

use serenity::all::{ChannelId, Http, MessageId, UserId};
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use crate::channels::common::{AllowList, StreamBuf};

/// One streaming reply in flight. Keyed by `stream_id` in
/// [`DiscordState::streams`].
pub(super) struct StreamEntry {
    /// Throttled accumulator for delta text.
    pub buf: StreamBuf,
    /// Discord message id of the initial-send message; populated
    /// after the first [`crate::channels::common::StreamAction::SendInitial`]
    /// completes successfully.
    pub message: Option<MessageId>,
    /// Discord channel where the initial send happened. Captured so
    /// later edits target the right DM even if `dm_channel` is updated
    /// by an interleaving inbound message in the same session.
    pub channel: Option<ChannelId>,
}

/// State shared between the Discord channel and its serenity handler.
pub(super) struct DiscordState {
    /// Bot's own user id. Filled in the `READY` event; used to filter
    /// out self-authored messages.
    pub bot_user_id: Mutex<Option<UserId>>,
    /// DM channel of the most recent allowed inbound message. Single-
    /// user MS0 model: the latest writer wins.
    pub dm_channel: Mutex<Option<ChannelId>>,
    /// Per-stream accumulators + sent-message handles.
    pub streams: Mutex<HashMap<Uuid, StreamEntry>>,
    /// Serenity HTTP handle. Set once the client is constructed in
    /// [`super::DiscordChannel::start`]; read by the outbound path.
    pub http: Mutex<Option<Arc<Http>>>,
    /// Allow list shared with [`super::DiscordControl`]. Both sides
    /// hold cheap-clone handles to the same underlying
    /// `Arc<RwLock<HashSet<u64>>>` — runtime mutations propagate
    /// without explicit notification.
    pub allowed: AllowList<u64>,
    /// Active flag. `true` ⇒ the supervisor loop should hold an open
    /// gateway connection. Mutated by
    /// [`super::DiscordControl::enable`] / [`super::DiscordControl::disable`].
    pub active: Arc<watch::Sender<bool>>,
    /// Bot token, resolved at the most recent enable-time. Read by
    /// the supervisor when (re)building the serenity client. Re-set
    /// on every enable so token rotation needs no restart.
    pub token: Arc<Mutex<Option<String>>>,
}

impl DiscordState {
    pub(super) fn new(
        allowed: AllowList<u64>,
        active: Arc<watch::Sender<bool>>,
        token: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            bot_user_id: Mutex::new(None),
            dm_channel: Mutex::new(None),
            streams: Mutex::new(HashMap::new()),
            http: Mutex::new(None),
            allowed,
            active,
            token,
        }
    }
}
