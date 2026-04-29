//! Shared mutable state for the WeChat channel and its control handle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use reqwest::Client;
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use crate::channels::common::AllowList;

use super::api::WechatCredentials;

/// State shared by [`super::WechatChannel`] and [`super::WechatControl`].
pub(super) struct WechatState {
    /// Allow list shared with the command/control layer.
    pub allowed: AllowList<String>,
    /// Desired connection state. The channel supervisor subscribes to
    /// this and opens/closes the iLink long-poll session.
    pub active: Arc<watch::Sender<bool>>,
    /// Credentials resolved at enable-time or QR-login time.
    pub credentials: Arc<Mutex<Option<WechatCredentials>>>,
    /// Live HTTP client while connected.
    pub client: Mutex<Option<Client>>,
    /// Most recent allowed DM peer. MS0 supports one allowed user, so
    /// "latest writer wins" is acceptable.
    pub reply_peer: Mutex<Option<String>>,
    /// iLink context tokens keyed by peer id.
    pub context_tokens: Mutex<HashMap<String, String>>,
    /// Message ids already observed in this process.
    pub seen_messages: Mutex<HashSet<String>>,
    /// Final-only streaming buffers. WeChat does not support editing
    /// sent messages, so deltas accumulate until `ReplyEnd`.
    pub streams: Mutex<HashMap<Uuid, String>>,
}

impl WechatState {
    pub(super) fn new(
        allowed: AllowList<String>,
        active: Arc<watch::Sender<bool>>,
        credentials: Arc<Mutex<Option<WechatCredentials>>>,
    ) -> Self {
        Self {
            allowed,
            active,
            credentials,
            client: Mutex::new(None),
            reply_peer: Mutex::new(None),
            context_tokens: Mutex::new(HashMap::new()),
            seen_messages: Mutex::new(HashSet::new()),
            streams: Mutex::new(HashMap::new()),
        }
    }
}
