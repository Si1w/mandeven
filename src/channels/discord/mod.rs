//! Discord channel — DM-only adapter built on `serenity`.
//!
//! Scope (MS0):
//!
//! - Listens only to direct messages from users whose Discord user
//!   id is in the runtime-mutable allow list (mutated via the
//!   `/discord allow|deny` slash command, persisted to
//!   `<data_dir>/discord/allowlist.json`). Guild messages and
//!   non-allowlisted DMs are silently dropped.
//! - Outbound `Reply` payloads are chunked to fit Discord's 2000-char
//!   limit and posted as one or more messages.
//! - Streaming `ReplyDelta` payloads use a throttled `send-then-edit`
//!   model: the first non-empty delta sends a fresh message, later
//!   deltas edit the same message at most once per
//!   [`STREAM_EDIT_INTERVAL`] until `ReplyEnd` triggers a final edit
//!   (plus any overflow chunks).
//! - `ThinkingDelta` payloads are dropped — chain-of-thought is not
//!   surfaced on Discord.
//!
//! Lifecycle: [`DiscordChannel::start`] runs a supervisor loop that
//! observes a `tokio::sync::watch<bool>` flag. Flipping the flag via
//! [`DiscordControl::enable`] / [`DiscordControl::disable`] (i.e. the
//! `/discord enable|disable` commands) opens or closes the gateway
//! connection without restarting the process.
//!
//! Multi-user routing is intentionally not yet supported: the gateway
//! today binds one [`crate::bus::SessionID`] per [`crate::bus::ChannelID`],
//! so two Discord users would share a session. Single-user installs
//! are correct; multi-user requires the gateway change tracked by the
//! multi-session TODO in `crate::channels`.

mod bridge;
mod control;
mod state;
pub mod store;

pub use control::{DiscordControl, DiscordStatus};
pub use store::{ALLOWLIST_FILENAME, DISCORD_SUBDIR, allowlist_path};

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serenity::Client;
use serenity::all::{ChannelId, CreateMessage, EditMessage, GatewayIntents, Http, MessageId};
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use crate::bus::{ChannelID, InboundSender, OutboundMessage, OutboundPayload};
use crate::channels::Channel;
use crate::channels::common::{AllowList, FinalizeResult, StreamAction, StreamBuf, split_message};
use crate::channels::error::{Error, Result};
use crate::config::DiscordConfig;

use self::bridge::Handler;
use self::state::{DiscordState, StreamEntry};

/// Discord per-message scalar-value limit (the platform's hard limit).
const DISCORD_MAX_MESSAGE_LEN: usize = 2000;

/// Minimum delay between consecutive edits on the same streamed
/// message. Stays under Discord's roughly 5-edits-per-5-seconds
/// per-channel rate limit while keeping the typing feel responsive.
const STREAM_EDIT_INTERVAL: Duration = Duration::from_secs(1);

/// Discord adapter implementing [`Channel`].
pub struct DiscordChannel {
    id: ChannelID,
    state: Arc<DiscordState>,
}

impl DiscordChannel {
    /// Construct a Discord channel + paired runtime control handle.
    ///
    /// The channel is **always** registered with the manager whether
    /// or not Discord is currently enabled — the connection itself
    /// is gated by the `active` watch flag inside the shared state.
    /// To auto-connect at boot, call
    /// [`DiscordControl::enable`] right after this returns when the
    /// config has `enabled = true`.
    ///
    /// `cfg.token_env` is captured into the control so each enable
    /// can re-read the env var (supports rotation).
    /// `initial_allowed` typically comes from
    /// [`store::load`]; `store_path` from [`store::allowlist_path`].
    #[must_use]
    pub fn build(
        id: ChannelID,
        cfg: &DiscordConfig,
        initial_allowed: HashSet<u64>,
        store_path: PathBuf,
    ) -> (Self, DiscordControl) {
        let allowed = AllowList::with_initial(initial_allowed);
        let (active_tx, _initial_rx) = watch::channel(false);
        let active = Arc::new(active_tx);
        let token: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let state = Arc::new(DiscordState::new(
            allowed.clone(),
            active.clone(),
            token.clone(),
        ));
        let channel = Self { id, state };
        let control =
            DiscordControl::new(allowed, store_path, active, token, cfg.token_env.clone());
        (channel, control)
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn id(&self) -> &ChannelID {
        &self.id
    }

    /// Supervisor loop. Waits for the `active` watch flag to flip
    /// `true`, then opens a serenity gateway connection and races
    /// `Client::start()` against the next `false` transition. When
    /// the user runs `/discord disable`, the supervisor calls
    /// `shard_manager.shutdown_all()` and loops back to wait. The
    /// loop exits cleanly only when the watch sender is dropped
    /// (i.e. the agent and control are gone — process shutdown).
    async fn start(&self, inbound: InboundSender) -> Result<()> {
        let mut active_rx = self.state.active.subscribe();

        loop {
            // Wait for active = true. `borrow_and_update` marks the
            // current value as observed so the next `changed().await`
            // only returns on actual transitions.
            while !*active_rx.borrow_and_update() {
                if active_rx.changed().await.is_err() {
                    return Ok(());
                }
            }

            let Some(token) = self.state.token.lock().await.clone() else {
                eprintln!("[discord] enable signaled without a token; resetting to disabled");
                let _ = self.state.active.send(false);
                continue;
            };

            if let Err(err) = self.run_session(&token, &inbound, &mut active_rx).await {
                eprintln!("[discord] session ended: {err}");
            }
            self.clear_session_state().await;
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let Some(http) = self.state.http.lock().await.clone() else {
            // start() not yet finished initializing the client. The
            // outbound message is dropped — for MS0 this only races
            // briefly at boot before the bot is ready.
            return Ok(());
        };
        let dm = *self.state.dm_channel.lock().await;

        match msg.payload {
            OutboundPayload::TurnEnd | OutboundPayload::ThinkingDelta { .. } => Ok(()),
            OutboundPayload::Reply(text) => match dm {
                Some(ch) => post_chunked(&http, ch, &text).await,
                None => Ok(()),
            },
            OutboundPayload::ReplyDelta { stream_id, delta } => match dm {
                Some(ch) => self.handle_delta(&http, ch, stream_id, &delta).await,
                None => Ok(()),
            },
            OutboundPayload::ReplyEnd { stream_id } => self.handle_end(&http, stream_id).await,
            OutboundPayload::Notice(text) => match dm {
                Some(ch) => post_chunked(&http, ch, &format!("_{text}_")).await,
                None => Ok(()),
            },
            OutboundPayload::Error(text) => match dm {
                Some(ch) => post_chunked(&http, ch, &format!("```\n{text}\n```")).await,
                None => Ok(()),
            },
            OutboundPayload::SessionSwitched(_) => match dm {
                Some(ch) => post_chunked(&http, ch, "_session switched_").await,
                None => Ok(()),
            },
        }
    }
}

impl DiscordChannel {
    /// Run one gateway session: build the serenity client, race its
    /// `start()` against the deactivate signal, and tear it down
    /// cleanly. Returns when either side ended; the supervisor loop
    /// decides whether to spin up another session.
    async fn run_session(
        &self,
        token: &str,
        inbound: &InboundSender,
        active_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let handler = Handler::new(self.id.clone(), self.state.clone(), inbound.clone());
        let intents = GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
        let mut client = Client::builder(token, intents)
            .event_handler(handler)
            .await
            .map_err(|e| map_serenity(&e))?;
        let shard_manager = client.shard_manager.clone();
        *self.state.http.lock().await = Some(client.http.clone());

        tokio::select! {
            result = client.start() => {
                if let Err(err) = result {
                    eprintln!("[discord] client.start exited: {err}");
                }
                // Gateway closed itself (network drop, auth failure,
                // etc.). Treat as an unsolicited disable so the user
                // can re-enable explicitly once they fix the cause.
                let _ = self.state.active.send(false);
            }
            () = wait_for_inactive(active_rx) => {
                shard_manager.shutdown_all().await;
            }
        }
        Ok(())
    }

    /// Reset all per-connection state after a session ends so the
    /// next enable starts clean.
    async fn clear_session_state(&self) {
        *self.state.http.lock().await = None;
        *self.state.bot_user_id.lock().await = None;
        *self.state.dm_channel.lock().await = None;
        self.state.streams.lock().await.clear();
    }

    async fn handle_delta(
        &self,
        http: &Http,
        dm: ChannelId,
        stream_id: Uuid,
        delta: &str,
    ) -> Result<()> {
        let action = {
            let mut streams = self.state.streams.lock().await;
            let entry = streams.entry(stream_id).or_insert_with(|| StreamEntry {
                buf: StreamBuf::new(STREAM_EDIT_INTERVAL, DISCORD_MAX_MESSAGE_LEN),
                message: None,
                channel: None,
            });
            entry.buf.append(delta)
        };

        match action {
            StreamAction::Buffer => Ok(()),
            StreamAction::SendInitial(content) => {
                let posted = dm
                    .send_message(http, CreateMessage::new().content(content))
                    .await
                    .map_err(|e| map_serenity(&e))?;
                let mut streams = self.state.streams.lock().await;
                if let Some(entry) = streams.get_mut(&stream_id) {
                    entry.message = Some(posted.id);
                    entry.channel = Some(dm);
                }
                Ok(())
            }
            StreamAction::Edit(content) => {
                let target = {
                    let streams = self.state.streams.lock().await;
                    streams
                        .get(&stream_id)
                        .and_then(|e| e.channel.zip(e.message))
                };
                if let Some((ch, mid)) = target {
                    ch.edit_message(http, mid, EditMessage::new().content(content))
                        .await
                        .map_err(|e| map_serenity(&e))?;
                }
                Ok(())
            }
        }
    }

    async fn handle_end(&self, http: &Http, stream_id: Uuid) -> Result<()> {
        let entry = self.state.streams.lock().await.remove(&stream_id);
        let Some(entry) = entry else { return Ok(()) };
        let StreamEntry {
            buf,
            message,
            channel,
        } = entry;

        let FinalizeResult { head, tail } = buf.finalize();
        let Some(head) = head else { return Ok(()) };
        let (Some(ch), Some(mid)) = (channel, message) else {
            return Ok(());
        };
        finalize_post(http, ch, mid, &head, &tail).await
    }
}

/// Block until the active flag is observed `false`. Returns
/// immediately if the value is already `false`, otherwise awaits the
/// next transition. Sender drop is treated as "deactivated" — the
/// process is shutting down, time to wind everything up.
async fn wait_for_inactive(rx: &mut watch::Receiver<bool>) {
    loop {
        if !*rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn finalize_post(
    http: &Http,
    ch: ChannelId,
    head_msg: MessageId,
    head: &str,
    tail: &[String],
) -> Result<()> {
    ch.edit_message(http, head_msg, EditMessage::new().content(head))
        .await
        .map_err(|e| map_serenity(&e))?;
    for chunk in tail {
        ch.send_message(http, CreateMessage::new().content(chunk))
            .await
            .map_err(|e| map_serenity(&e))?;
    }
    Ok(())
}

async fn post_chunked(http: &Http, ch: ChannelId, text: &str) -> Result<()> {
    for chunk in split_message(text, DISCORD_MAX_MESSAGE_LEN) {
        ch.send_message(http, CreateMessage::new().content(chunk))
            .await
            .map_err(|e| map_serenity(&e))?;
    }
    Ok(())
}

/// Funnel any serenity error into [`Error::Io`]. The adapter does not
/// branch on serenity error variants today, so the type loss is
/// acceptable; the manager logs the message and the channel stays
/// registered.
fn map_serenity(e: &serenity::Error) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}
