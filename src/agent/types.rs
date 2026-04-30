//! Value types used across the agent module.

use crate::bus::{ChannelID, SessionID};
use crate::exec::ExecId;
use crate::llm::{FinishReason, ToolCall, Usage};

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
    /// Machine-readable execution log to append events to, when this
    /// iteration was started by a scheduled task.
    pub exec_id: Option<ExecId>,
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
