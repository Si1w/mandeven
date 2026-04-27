//! Message and identifier types carried on the bus.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// UUID-based identifier for a single message on the bus.
///
/// Uses UUID v7 so identifiers are chronologically sortable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageID(pub Uuid);

impl MessageID {
    /// Generate a fresh identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MessageID {
    fn default() -> Self {
        Self::new()
    }
}

/// UUID-based identifier for a conversation session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionID(pub Uuid);

impl SessionID {
    /// Generate a fresh identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SessionID {
    fn default() -> Self {
        Self::new()
    }
}

/// Human-readable identifier for a channel (for example `"cli"`,
/// `"tui"`, `"cron"`).
///
/// `#[serde(transparent)]` so session-metadata JSONL stores the
/// channel as a bare string (`"channel": "tui"`) rather than a
/// wrapper object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ChannelID(pub String);

impl ChannelID {
    /// Wrap any string-like value as a channel identifier.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Borrow the underlying label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A message flowing from a channel into the gateway.
///
/// Carries only **identity** fields — no `SessionID`. The gateway is
/// the session authority: it derives or looks up the binding between
/// `(channel, peer, account, guild)` and a concrete `SessionID`
/// before forwarding the message into the agent loop as a
/// [`crate::gateway::ResolvedInboundMessage`]. This mirrors the
/// routing model used in `agent-examples/claw0` — channels know
/// *who* sent the message, not *which historical session* it belongs
/// to.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Unique identifier, also serves as a stable ordering key.
    pub id: MessageID,
    /// When the message was created.
    pub timestamp: DateTime<Utc>,
    /// Channel that produced this message.
    pub channel: ChannelID,
    /// Platform-specific user identity. CLI fills a constant (there
    /// is only one user per terminal); future IM channels fill the
    /// platform-provided user id. `None` is reserved for channels that
    /// don't carry a meaningful peer concept (broadcast / system
    /// channels); current code does not exercise it.
    pub peer_id: Option<String>,
    /// Bot / workspace / account identity on multi-tenant platforms.
    /// Unused by the CLI; reserved for tier-3 routing (see claw0's
    /// `BindingTable`).
    pub account_id: Option<String>,
    /// Guild / server identity on platforms that group chats into
    /// guilds (Discord servers, Slack workspaces). Reserved for
    /// tier-2 routing.
    pub guild_id: Option<String>,
    /// Variant-specific payload.
    pub payload: InboundPayload,
}

impl InboundMessage {
    /// Construct an inbound message with a fresh id and timestamp.
    /// Identity fields beyond `channel` default to `None`; callers
    /// that know who the peer is should use [`Self::with_peer`].
    #[must_use]
    pub fn new(channel: ChannelID, payload: InboundPayload) -> Self {
        Self {
            id: MessageID::new(),
            timestamp: Utc::now(),
            channel,
            peer_id: None,
            account_id: None,
            guild_id: None,
            payload,
        }
    }

    /// Construct an inbound message tagged with a peer identity.
    /// Used by the CLI channel (peer = a constant representing the
    /// local user) and future IM channels (peer = platform user id).
    #[must_use]
    pub fn with_peer(
        channel: ChannelID,
        peer_id: impl Into<String>,
        payload: InboundPayload,
    ) -> Self {
        let mut msg = Self::new(channel, payload);
        msg.peer_id = Some(peer_id.into());
        msg
    }
}

/// Variants of inbound content.
#[derive(Debug, Clone)]
pub enum InboundPayload {
    /// Text typed by the user.
    UserInput(String),
    /// Slash command the channel did not handle locally, forwarded to
    /// the agent's router. The string is the command body with the
    /// leading `/` already stripped (for example `"ping"` or
    /// `"status verbose"`).
    Command(String),
}

/// A message flowing from the agent out to a channel.
#[derive(Debug, Clone)]
pub struct OutboundMessage {
    /// Unique identifier, also serves as a stable ordering key.
    pub id: MessageID,
    /// When the message was created.
    pub timestamp: DateTime<Utc>,
    /// Channel that should render this message.
    pub channel: ChannelID,
    /// Conversation this message belongs to.
    pub session: SessionID,
    /// Variant-specific payload.
    pub payload: OutboundPayload,
}

impl OutboundMessage {
    /// Construct an outbound message with a fresh id and timestamp.
    #[must_use]
    pub fn new(channel: ChannelID, session: SessionID, payload: OutboundPayload) -> Self {
        Self {
            id: MessageID::new(),
            timestamp: Utc::now(),
            channel,
            session,
            payload,
        }
    }
}

/// Variants of outbound content.
#[derive(Debug, Clone)]
pub enum OutboundPayload {
    /// Complete assistant reply, non-streaming path.
    Reply(String),
    /// Streaming assistant reply fragment.
    ReplyDelta {
        /// Groups deltas belonging to the same streaming response.
        stream_id: Uuid,
        /// Incremental text fragment.
        delta: String,
    },
    /// Streaming chain-of-thought fragment from a thinking-capable
    /// model. Shares its `stream_id` with the matching
    /// [`Self::ReplyDelta`] sequence so a channel can group both
    /// streams under the same turn.
    ThinkingDelta {
        /// Same `stream_id` as the matching `ReplyDelta` / `ReplyEnd`.
        stream_id: Uuid,
        /// Incremental reasoning fragment.
        delta: String,
    },
    /// End marker for a streaming response.
    ReplyEnd {
        /// Identifier of the finished stream; matches its deltas.
        stream_id: Uuid,
    },
    /// End marker for a full user turn, after all model/tool loops
    /// are complete. Channels should use this, not [`Self::ReplyEnd`],
    /// to leave their busy state or drain queued follow-up input.
    TurnEnd,
    /// Error surfaced to the channel for display.
    Error(String),
    /// Ambient system message from the agent or a command handler —
    /// neither a model reply nor an error. Used for command feedback
    /// and similar non-conversational notices.
    Notice(String),
    /// The channel's active session binding has changed (for example
    /// after `/new` or `/load`). The channel is expected to re-render
    /// its transcript against the new session — typically by reading
    /// history from its [`crate::session::Manager`] handle. A
    /// separate `Notice` is usually sent alongside to explain the
    /// switch to the user.
    SessionSwitched(SessionID),
}
