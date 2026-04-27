//! Gateway — the session authority that sits between channels and
//! the agent loop.
//!
//! Every [`crate::bus::InboundMessage`] coming from a channel lands
//! here first. The gateway:
//!
//! 1. If the payload is an [`InboundPayload::Command`], tries the
//!    gateway-level [`commands`] handlers first. A hit is handled in
//!    place — the gateway emits a [`OutboundPayload::Notice`] (and
//!    a [`OutboundPayload::SessionSwitched`] when the binding
//!    changed) and the message does **not** reach the agent.
//! 2. Otherwise (or when the command belongs to a later layer), looks up
//!    — or creates —
//!    the [`crate::bus::SessionID`] bound to the message's channel,
//!    and forwards the message to the agent as an
//!    [`InboundDispatch`].
//!
//! The design follows `agent-examples/claw0`'s Gateway +
//! `BindingTable` split: identity lives in the transport payload,
//! session assignment is a gateway concern, and routing-style
//! commands (`/new`, `/list`, `/load`) live in the gateway rather than
//! in either the channel or the agent.

pub mod bindings;
pub mod commands;

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::bus::{
    ChannelID, InboundMessage, InboundPayload, InboundReceiver, MessageID, OutboundMessage,
    OutboundPayload, OutboundSender, Receiver, Sender, SessionID,
};
use crate::command::CommandOutcome;
use crate::command::slash::{self, SlashCommand};
use crate::session;

use self::commands::GatewayCommandCtx;

/// An [`InboundMessage`] after the gateway has attached a concrete
/// [`SessionID`]. Agent-loop code consumes this rather than
/// [`InboundMessage`] directly; the type distinction makes the
/// "session has been attached" invariant explicit.
///
/// Named `InboundDispatch` to align with the `dispatch` vocabulary
/// used elsewhere in the codebase (`CliChannel::dispatch_command`):
/// this is the payload the gateway dispatches onward to the agent.
#[derive(Debug, Clone)]
pub struct InboundDispatch {
    /// Identifier propagated from the inbound message.
    pub id: MessageID,
    /// Timestamp propagated from the inbound message.
    pub timestamp: DateTime<Utc>,
    /// Originating channel.
    pub channel: ChannelID,
    /// Session the gateway attached to this message.
    pub session: SessionID,
    /// Payload propagated from the inbound message.
    pub payload: InboundPayload,
}

/// Sender half of the gateway → agent queue.
pub type DispatchSender = Sender<InboundDispatch>;

/// Receiver half of the gateway → agent queue.
pub type DispatchReceiver = Receiver<InboundDispatch>;

/// Create a fresh gateway-to-agent queue pair. Capacity matches the
/// bus queues so back-pressure behavior stays symmetric across the
/// two hops.
#[must_use]
pub fn dispatch_channel() -> (DispatchSender, DispatchReceiver) {
    const DISPATCH_CAPACITY: usize = 20;
    let (tx, rx) = tokio::sync::mpsc::channel(DISPATCH_CAPACITY);
    (Sender::from_raw(tx), Receiver::from_raw(rx))
}

/// Bindings from a channel to its currently active storage session.
/// Owned by the gateway as the binding authority but cloned out to
/// other subsystems (e.g. [`crate::heartbeat`], so a heartbeat tick
/// can run inside the user's main session rather than spinning up an
/// isolated one). `/new` and `/load` mutate the map; readers see the
/// switch automatically through the shared `Arc`.
pub type ActiveSessions = Arc<Mutex<HashMap<ChannelID, SessionID>>>;

/// Gateway — owns the inbound routing loop plus the session
/// binding table.
pub struct Gateway {
    /// See [`ActiveSessions`].
    active_sessions: ActiveSessions,
    /// Per-channel snapshot of the most recent `/list` output, used
    /// by `/load <n>` to map a numeric index to a `SessionID`.
    last_listed: Arc<Mutex<HashMap<ChannelID, Vec<SessionID>>>>,
    /// Session store handle; the gateway reads metadata for `/list`
    /// and the channel reads message history when reacting to a
    /// `SessionSwitched` notice.
    sessions: Arc<session::Manager>,
    /// Inbound stream from channels.
    inbound: InboundReceiver,
    /// Forward stream to the agent loop.
    forward: DispatchSender,
    /// Outbound sender, used to emit gateway-level command feedback
    /// (notices, session-switch announcements) back to channels.
    outbound: OutboundSender,
}

impl Gateway {
    /// Construct a gateway wired to the given bus halves.
    ///
    /// Caller is responsible for spawning [`Self::run`] and
    /// eventually dropping the matching senders so the loop exits.
    #[must_use]
    pub fn new(
        inbound: InboundReceiver,
        forward: DispatchSender,
        outbound: OutboundSender,
        sessions: Arc<session::Manager>,
        active_sessions: ActiveSessions,
    ) -> Self {
        Self {
            active_sessions,
            last_listed: Arc::new(Mutex::new(HashMap::new())),
            sessions,
            inbound,
            forward,
            outbound,
        }
    }

    /// Drive the inbound loop until the inbound stream closes.
    ///
    /// # Errors
    ///
    /// Returns `Ok(())` when the inbound stream closes cleanly.
    /// Propagates [`crate::bus::Error::Closed`] if the forward or
    /// outbound queues are dropped before the inbound stream closes
    /// — meaning either the agent or the channel layer is gone and
    /// the gateway has nowhere to send messages.
    pub async fn run(mut self) -> crate::bus::Result<()> {
        while let Some(msg) = self.inbound.recv().await {
            // Branch on whether this is a slash command we own.
            // Anything else (UserInput, agent-targeted commands)
            // gets a session attached and forwards onward.
            if let InboundPayload::Command(body) = &msg.payload
                && self.try_dispatch_gateway_command(&msg, body).await?
            {
                continue;
            }

            let session = self.session_for(&msg).await;
            let dispatch = InboundDispatch {
                id: msg.id,
                timestamp: msg.timestamp,
                channel: msg.channel,
                session,
                payload: msg.payload,
            };
            self.forward.send(dispatch).await?;
        }
        Ok(())
    }

    /// Try to handle one slash command at the gateway layer.
    ///
    /// Returns `Ok(true)` when the command matched (and the message
    /// should not be forwarded onward), `Ok(false)` when the
    /// command belongs to a later layer (the message continues to
    /// the agent so it can have a chance).
    /// Propagates outbound bus errors.
    ///
    /// On a hit the gateway reads `active_sessions` *before* and
    /// *after* dispatch to detect a binding change; if the session
    /// for this channel changed, an extra
    /// [`OutboundPayload::SessionSwitched`] notice is emitted so
    /// the channel can rebuild its transcript.
    async fn try_dispatch_gateway_command(
        &self,
        msg: &InboundMessage,
        body: &str,
    ) -> crate::bus::Result<bool> {
        let parsed = match slash::parse(body) {
            Ok(parsed) => parsed,
            Err(err) => {
                if let Some(payload) =
                    command_outcome_to_outbound(CommandOutcome::Feedback(err), &msg.channel)
                {
                    self.outbound.send(payload).await?;
                }
                return Ok(true);
            }
        };

        let is_gateway_command = matches!(
            parsed,
            SlashCommand::New | SlashCommand::List | SlashCommand::Load { .. }
        );
        if !is_gateway_command {
            return Ok(false);
        }

        let before = self.bound_session(&msg.channel).await;
        let ctx = GatewayCommandCtx {
            channel: msg.channel.clone(),
            active_sessions: self.active_sessions.clone(),
            last_listed: self.last_listed.clone(),
            sessions: self.sessions.clone(),
        };
        let outcome = match parsed {
            SlashCommand::New => ctx.new_session().await,
            SlashCommand::List => ctx.list_sessions().await,
            SlashCommand::Load { index } => ctx.load_session(index).await,
            _ => unreachable!("gateway command prechecked"),
        };
        let after = self.bound_session(&msg.channel).await;

        if before != after
            && let Some(new_id) = after
        {
            let switched = OutboundMessage::new(
                msg.channel.clone(),
                new_id.clone(),
                OutboundPayload::SessionSwitched(new_id),
            );
            self.outbound.send(switched).await?;
        }

        if let Some(payload) = command_outcome_to_outbound(outcome, &msg.channel) {
            self.outbound.send(payload).await?;
        }

        Ok(true)
    }

    /// Snapshot the channel's currently bound session, if any.
    async fn bound_session(&self, channel: &ChannelID) -> Option<SessionID> {
        self.active_sessions.lock().await.get(channel).cloned()
    }

    /// Look up — or lazily create — the session bound to this
    /// message's channel. Used by the forward path; gateway
    /// commands mutate the same map but go through
    /// [`GatewayCommandCtx`] helpers.
    async fn session_for(&self, msg: &InboundMessage) -> SessionID {
        let mut map = self.active_sessions.lock().await;
        map.entry(msg.channel.clone())
            .or_insert_with(SessionID::new)
            .clone()
    }
}

/// Translate a command outcome into the outbound message the channel
/// should see. `Completed` means "no message" (the side effect already
/// happened, gateway has nothing to add). `Exit` is meaningless at
/// the gateway layer and is dropped with a `[gateway]` log line.
/// `Feedback` becomes a [`OutboundPayload::Notice`] so the channel
/// renders it as ambient text.
fn command_outcome_to_outbound(
    outcome: CommandOutcome,
    channel: &ChannelID,
) -> Option<OutboundMessage> {
    let payload = match outcome {
        CommandOutcome::Completed => return None,
        CommandOutcome::Feedback(text) => OutboundPayload::Notice(text),
        CommandOutcome::Exit => {
            eprintln!("[gateway] command returned Exit at gateway layer; ignoring");
            return None;
        }
    };
    // OutboundMessage requires a session id; gateway-level feedback
    // is conceptually session-less, so we tag it with a freshly
    // generated id (the channel does not currently filter on it).
    Some(OutboundMessage::new(
        channel.clone(),
        SessionID::new(),
        payload,
    ))
}
