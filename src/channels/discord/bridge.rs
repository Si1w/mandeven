//! Bridge between serenity gateway events and the inbound bus.
//!
//! [`Handler`] implements serenity's `EventHandler`. It captures the
//! bot's own user id on `READY`, then turns each inbound DM into an
//! [`InboundMessage`] published on the bus — provided the sender is in
//! the shared allow list and the message did not arrive in a guild
//! channel.
//!
//! The handler reads the allow list straight off the shared
//! [`DiscordState`]; runtime mutations made by `/discord allow|deny`
//! land via the same `Arc<RwLock<...>>` and take effect on the next
//! inbound event without any explicit signal.

use std::sync::Arc;

use serenity::all::{Context, EventHandler, Message, Ready};
use serenity::async_trait;

use crate::bus::{ChannelID, InboundMessage, InboundPayload, InboundSender};

use super::state::DiscordState;

/// Serenity event handler for the Discord adapter.
pub(super) struct Handler {
    channel_id: ChannelID,
    inbound: InboundSender,
    state: Arc<DiscordState>,
}

impl Handler {
    pub(super) fn new(
        channel_id: ChannelID,
        state: Arc<DiscordState>,
        inbound: InboundSender,
    ) -> Self {
        Self {
            channel_id,
            inbound,
            state,
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        *self.state.bot_user_id.lock().await = Some(ready.user.id);
    }

    async fn message(&self, _ctx: Context, msg: Message) {
        // Self-loop guard. If the READY event has not landed yet the
        // first inbound message is dropped — fine, that race only
        // affects the moment of connection.
        if Some(msg.author.id) == *self.state.bot_user_id.lock().await {
            return;
        }
        // DM-only. Guild messages are silently dropped — the bot is
        // not authorized to participate in servers in MS0.
        if msg.guild_id.is_some() {
            return;
        }
        // Allowlist check reads the same shared `AllowList` that
        // `DiscordControl` mutates — no notification needed.
        if !self.state.allowed.is_allowed(&msg.author.id.get()) {
            return;
        }

        // Remember where to reply. Single-user MS0 model: latest
        // allowed writer wins. Full multi-user support also needs
        // per-peer outbound routing state in this adapter.
        *self.state.dm_channel.lock().await = Some(msg.channel_id);

        let payload = if let Some(body) = msg.content.strip_prefix('/') {
            InboundPayload::Command(body.to_string())
        } else {
            InboundPayload::UserInput(msg.content.clone())
        };

        let inbound =
            InboundMessage::with_peer(self.channel_id.clone(), msg.author.id.to_string(), payload);
        if let Err(err) = self.inbound.send(inbound).await {
            eprintln!("[discord] inbound bus closed, dropping message: {err}");
        }
    }
}
