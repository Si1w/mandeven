//! Value types used across the agent module.

use crate::bus::{ChannelID, SessionID};
use crate::exec::ExecId;
use crate::llm::{FinishReason, ToolCall, Usage};

/// Which session store an iteration should persist into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    /// The active project bucket.
    Foreground,
    /// The global cron/background bucket.
    Cron,
}

/// How an iteration should surface model output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Stream deltas and notices to the iteration channel.
    Visible,
    /// Persist the transcript only; do not send outbound messages.
    Silent,
}

/// Identifier for a single iteration — session plus source channel.
///
/// An iteration is one outer-loop cycle: receive one user message, run
/// any number of LLM calls with interleaved tool dispatches, and emit
/// the resulting reply. Passed into per-iteration helpers instead of
/// threading `SessionID` and `ChannelID` positionally through every
/// signature.
#[derive(Debug, Clone)]
pub struct Iteration {
    /// Conversation this iteration belongs to.
    pub session: SessionID,
    /// Channel that produced the user input and will receive the reply.
    pub channel: ChannelID,
    /// Platform-specific user identity, when available.
    pub peer_id: Option<String>,
    /// Bot / workspace / account identity, when available.
    pub account_id: Option<String>,
    /// Guild / server identity, when available.
    pub guild_id: Option<String>,
    /// Session store that owns this iteration's transcript.
    pub scope: SessionScope,
    /// Output delivery behavior.
    pub delivery: DeliveryMode,
    /// Machine-readable execution log to append events to, when this
    /// iteration was started by a scheduled task.
    pub exec_id: Option<ExecId>,
}

impl Iteration {
    /// Construct a normal user-visible iteration in the foreground
    /// project bucket.
    #[must_use]
    pub fn visible(session: SessionID, channel: ChannelID, exec_id: Option<ExecId>) -> Self {
        Self::visible_with_identity(session, channel, None, None, None, exec_id)
    }

    /// Construct a normal user-visible iteration with full inbound
    /// identity metadata.
    #[must_use]
    pub fn visible_with_identity(
        session: SessionID,
        channel: ChannelID,
        peer_id: Option<String>,
        account_id: Option<String>,
        guild_id: Option<String>,
        exec_id: Option<ExecId>,
    ) -> Self {
        Self {
            session,
            channel,
            peer_id,
            account_id,
            guild_id,
            scope: SessionScope::Foreground,
            delivery: DeliveryMode::Visible,
            exec_id,
        }
    }

    /// Construct a silent background iteration in the cron bucket.
    #[must_use]
    pub fn silent_cron(session: SessionID, channel: ChannelID, exec_id: Option<ExecId>) -> Self {
        Self {
            session,
            channel,
            peer_id: None,
            account_id: None,
            guild_id: None,
            scope: SessionScope::Cron,
            delivery: DeliveryMode::Silent,
            exec_id,
        }
    }

    /// Whether outbound messages should be sent to the channel layer.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.delivery == DeliveryMode::Visible
    }
}

/// Outcome of a single LLM call within an iteration.
///
/// An iteration consists of one or more calls with intervening tool
/// dispatches. When `tool_calls` is `Some` and non-empty, the agent
/// dispatches those tools, appends `Message::Tool` replies, and issues
/// another call. When `None` or empty, the iteration is complete.
#[derive(Debug, Clone)]
pub struct CallOutcome {
    /// Text accumulated from the stream; empty if the model only
    /// invoked tools without emitting any text.
    pub content: String,
    /// Reasoning trace accumulated from the stream when the call
    /// asked for thinking and the provider supports it. Stored on
    /// the persisted `Message::Assistant` so subsequent turns can
    /// replay it back to the API (`DeepSeek` requires this on tool
    /// turns).
    pub thinking: Option<String>,
    /// Tool invocations emitted by this call, if any.
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Why the model stopped (`FinishReason::ToolCalls` for
    /// tool-invoking stops).
    pub finish_reason: FinishReason,
    /// Token accounting for this call, populated only on the final
    /// chunk of the stream.
    pub usage: Option<Usage>,
}
