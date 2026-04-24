//! Outbound router + channel lifecycle owner.
//!
//! [`Manager`] is the one place that holds the single
//! [`crate::bus::OutboundReceiver`]. It spawns each registered
//! channel's `start` task (with its own clone of the inbound sender)
//! and loops on the outbound bus, forwarding every message to the
//! channel whose [`crate::bus::ChannelID`] matches
//! `msg.channel`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::bus::{ChannelID, InboundSender, OutboundReceiver};

use super::Channel;
use super::error::{Error, Result};

/// Owns the single outbound receiver and the set of registered
/// channels.
pub struct Manager {
    channels: HashMap<ChannelID, Arc<dyn Channel>>,
    outbound_rx: OutboundReceiver,
}

impl Manager {
    /// Construct a manager that will route messages from
    /// `outbound_rx`.
    #[must_use]
    pub fn new(outbound_rx: OutboundReceiver) -> Self {
        Self {
            channels: HashMap::new(),
            outbound_rx,
        }
    }

    /// Register a channel. Registration is static — every channel
    /// must be installed before [`Self::run`] is called. A later
    /// registration under the same [`ChannelID`] overwrites the
    /// earlier entry.
    pub fn register(&mut self, channel: Arc<dyn Channel>) {
        let id = channel.id().clone();
        self.channels.insert(id, channel);
    }

    /// Consume `self`: spawn each registered channel's
    /// `start(inbound.clone())`, then loop on the outbound bus
    /// delivering messages via `channel.send`. Returns when the
    /// outbound bus closes (i.e. the agent drops its sender), after
    /// joining every channel task.
    ///
    /// Messages whose [`ChannelID`] is not registered are logged and
    /// dropped. A `send()` failure is logged — the channel stays in
    /// the routing table so a transient error does not disable the
    /// channel permanently.
    ///
    /// # Errors
    ///
    /// Returns the first [`Error`] surfaced by any channel's
    /// `start`. Other channels are still joined before this function
    /// returns; their errors (if any) are logged to stderr.
    //
    // TODO(concurrent-send): sequential `channel.send(msg).await` is
    // correct (it preserves the backpressure chain from channel →
    // bus → agent) but blocks the routing loop on any slow channel.
    // When external channels land (discord / slack / HTTP) and
    // head-of-line blocking between channels hurts, move to
    // per-channel routing tasks — each channel gets its own outbound
    // queue drained independently, backpressure stays intact.
    // Do NOT naïvely `tokio::spawn` per message: that drops
    // backpressure and the JoinSet grows unbounded.
    //
    // TODO(shutdown): shutdown today relies on the drop chain
    // (channel `start` exits → inbound closes → agent exits →
    // outbound closes → manager exits → remaining channel tasks are
    // joined). When soft-shutdown or mid-LLM-call interruption is
    // needed, add `tokio_util::sync::CancellationToken` and a
    // `Channel::stop()` cleanup hook.
    //
    // TODO(retry): on `channel.send()` failure, nanobot retries three
    // times with exponential backoff (1s / 2s / 4s). Wire a
    // per-channel retry policy when an external channel actually
    // needs it; local channels (tui) rarely fail and drop-with-log
    // is adequate for MS0.
    pub async fn run(self, inbound: InboundSender) -> Result<()> {
        let Self {
            channels,
            mut outbound_rx,
        } = self;

        // Spawn each channel's listener with its own inbound sender
        // clone, then drop our copy so the bus closes once every
        // listener exits.
        let mut listeners = JoinSet::new();
        for channel in channels.values() {
            let ch = channel.clone();
            let tx = inbound.clone();
            listeners.spawn(async move { ch.start(tx).await });
        }
        drop(inbound);

        // Outbound routing loop.
        while let Some(msg) = outbound_rx.recv().await {
            let target = msg.channel.clone();
            match channels.get(&target) {
                Some(ch) => {
                    if let Err(err) = ch.send(msg).await {
                        eprintln!("[channels] send to {target:?} failed: {err}");
                    }
                }
                None => {
                    eprintln!("[channels] outbound for unregistered channel {target:?} dropped");
                }
            }
        }

        // Outbound closed — wait for every listener to finish.
        let mut first_err: Option<Error> = None;
        while let Some(join_res) = listeners.join_next().await {
            match join_res {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    if first_err.is_none() {
                        first_err = Some(err);
                    } else {
                        eprintln!("[channels] channel listener error: {err}");
                    }
                }
                Err(join_err) => {
                    eprintln!("[channels] channel task join failed: {join_err}");
                }
            }
        }

        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}
