//! Data types exchanged between the agent and an LLM client.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};

use super::error::Result;

/// Role of a message. Derived from [`Message`] variants via [`Message::role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in a chat-completion exchange.
///
/// Each variant carries only the fields semantically valid for its
/// role, so illegal combinations are prevented at the type level.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// System prompt shaping assistant behavior.
    System {
        /// System prompt text.
        content: String,
    },
    /// User-authored input.
    User {
        /// User-authored text.
        content: String,
    },
    /// Model-authored output.
    ///
    /// `content` is optional because an assistant message that only
    /// invokes tools carries no text. `tool_calls` is populated only
    /// when the model invokes one or more tools.
    Assistant {
        /// Assistant-authored text, if any.
        content: Option<String>,
        /// Tool invocations, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    /// Reply to an earlier [`Self::Assistant`] tool invocation.
    Tool {
        /// Tool result text.
        content: String,
        /// `id` of the originating [`ToolCall`] in the assistant message.
        tool_call_id: String,
    },
}

impl Message {
    /// Role projection.
    #[must_use]
    pub fn role(&self) -> Role {
        match self {
            Self::System { .. } => Role::System,
            Self::User { .. } => Role::User,
            Self::Assistant { .. } => Role::Assistant,
            Self::Tool { .. } => Role::Tool,
        }
    }
}

/// Tool schema advertised to the model.
///
/// Placeholder — fields defined by the agent module alongside the
/// tool-use contract. Reserved here so downstream modules share the
/// same nominal type.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool;

/// Tool invocation emitted by the model in response to an available
/// [`Tool`].
///
/// Placeholder — fields defined by the agent module.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall;

/// Partial [`ToolCall`] fragment emitted during streaming.
///
/// Placeholder — fields defined by the agent module alongside the
/// streaming tool-call state machine.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallDelta;

/// Trait implemented by skills that expose a [`Tool`] to the model.
///
/// Placeholder — methods defined by the agent module.
pub trait BaseTool: Send + Sync {}

/// A completion request.
///
/// Provider and profile resolution is the caller's job; this struct
/// only carries the wire payload shape.
#[derive(Debug, Clone)]
pub struct Request {
    /// Conversation so far, in chronological order.
    pub messages: Vec<Message>,
    /// Model identifier sent in the request body
    /// (for example `"mistral-small-latest"`).
    pub model_name: String,
    /// Upper bound on completion tokens.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
}

/// Non-streaming completion response.
#[derive(Debug, Clone)]
pub struct Response {
    /// Assistant-authored text. `None` if the model only invoked tools.
    pub content: Option<String>,
    /// Tool invocations, if any.
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Token accounting for this request.
    pub usage: Usage,
    /// Why the model stopped producing tokens.
    pub finish_reason: FinishReason,
}

/// One incremental chunk in a streaming response.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Incremental text fragment; `None` on chunks that carry only
    /// finish metadata.
    pub content_delta: Option<String>,
    /// Per-tool-call partial updates, if present in this chunk.
    pub tool_call_deltas: Option<Vec<ToolCallDelta>>,
    /// Populated only on the final chunk.
    pub finish_reason: Option<FinishReason>,
    /// Populated only on the final chunk; some providers omit entirely.
    pub usage: Option<Usage>,
}

/// A pinned, boxed stream of [`StreamChunk`] items yielded by the
/// client's streaming entry point.
pub type ResponseStream = Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>;

/// Token accounting for a completion.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    /// Tokens consumed by the prompt (input).
    pub prompt_tokens: u32,
    /// Tokens produced by the completion (output).
    pub completion_tokens: u32,
    /// Sum of prompt and completion tokens.
    pub total_tokens: u32,
}

/// Why the model stopped producing tokens.
#[derive(Debug, Clone)]
pub enum FinishReason {
    /// Natural stop (end of turn or stop sequence hit).
    Stop,
    /// Completion hit `max_tokens`.
    Length,
    /// Model invoked one or more tools and yielded for their results.
    ToolCalls,
    /// Provider-specific reason passed through verbatim.
    Other(String),
}
