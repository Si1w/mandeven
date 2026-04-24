//! Message and identifier types carried on the bus.

use chrono::{DateTime, Utc};
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

/// A message flowing from a channel into the agent.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Unique identifier, also serves as a stable ordering key.
    pub id: MessageID,
    /// When the message was created.
    pub timestamp: DateTime<Utc>,
    /// Channel that produced this message.
    pub channel: ChannelID,
    /// Conversation this message belongs to.
    pub session: SessionID,
    /// Variant-specific payload.
    pub payload: InboundPayload,
}

impl InboundMessage {
    /// Construct an inbound message with a fresh id and timestamp.
    #[must_use]
    pub fn new(channel: ChannelID, session: SessionID, payload: InboundPayload) -> Self {
        Self {
            id: MessageID::new(),
            timestamp: Utc::now(),
            channel,
            session,
            payload,
        }
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
    /// End marker for a streaming response.
    ReplyEnd {
        /// Identifier of the finished stream; matches its deltas.
        stream_id: Uuid,
    },
    /// Error surfaced to the channel for display.
    Error(String),
    /// Ambient system message from the agent or a command handler —
    /// neither a model reply nor an error. Used for command feedback
    /// (`/ping` → `"pong"`) and similar non-conversational notices.
    Notice(String),
}
