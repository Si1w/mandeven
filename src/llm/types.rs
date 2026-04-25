//! Data types exchanged between the agent and an LLM client.

use futures::stream::BoxStream;
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
        /// Chain-of-thought trace from a reasoning-capable model
        /// (`DeepSeek`'s `reasoning_content`, Anthropic's thinking
        /// blocks, `OpenAI` o-series reasoning, …). Always `None` for
        /// providers that do not surface reasoning.
        ///
        /// **Must be preserved across turns** when the message also
        /// contains `tool_calls` — `DeepSeek` requires the exact
        /// `reasoning_content` to be replayed in subsequent requests
        /// or it returns 400. Storing it on the assistant message is
        /// what makes [`crate::session`] persistence + agent replay
        /// transparently correct.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
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

/// JSON-Schema-described function tool advertised to the model.
///
/// Flat shape: the `OpenAI`-style `{type:"function", function:{...}}`
/// wrapper is provider-specific and applied at serialization time by
/// the provider implementation (see [`super::providers`]).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    /// Identifier the model uses to invoke the tool.
    pub name: String,
    /// Human-readable description shown to the model.
    pub description: String,
    /// JSON Schema describing the arguments object. Typically produced
    /// by `schemars` from a parameters struct.
    pub parameters: serde_json::Value,
}

/// Tool invocation emitted by the model in response to an advertised
/// [`Tool`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall {
    /// Provider-generated identifier; echoed back in the matching
    /// [`Message::Tool`] reply.
    pub id: String,
    /// Name of the invoked tool.
    pub name: String,
    /// Raw JSON string as emitted by the model. Preserved verbatim for
    /// session-history fidelity and parsed at dispatch time.
    pub arguments: String,
}

/// Streaming fragment of a [`ToolCall`].
///
/// The model may emit multiple tool calls in parallel; `index`
/// identifies which accumulating call this fragment updates. `id` and
/// `name` typically appear on the first fragment for an index;
/// `arguments` is concatenated across fragments as JSON text.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallDelta {
    /// Position of the tool call in the emitted batch.
    pub index: u32,
    /// Present on the first fragment for this index.
    pub id: Option<String>,
    /// Present on the first fragment for this index.
    pub name: Option<String>,
    /// Incremental JSON text to append to the accumulating arguments.
    pub arguments: Option<String>,
}

/// Optional effort hint inside a [`Thinking`] block.
///
/// The field name `reasoning_effort` matches `OpenAI` o-series and the
/// equivalent knob in openclaw / `DeepSeek`'s Anthropic-format
/// `output_config.effort`. The `OpenAI`-format `DeepSeek` endpoint has
/// no effort knob, so providers using that wire format drop the hint
/// on serialize. `None` lets the provider pick its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    High,
    Max,
}

/// Thinking-mode configuration for one request.
///
/// Field shape mirrors `DeepSeek`'s `extra_body.thinking` block: an
/// explicit on/off switch (`enabled`) plus an optional effort level.
/// Carrying it as `Option<Thinking>` on [`Request`] gives three
/// distinct call sites:
///
/// - `None` — caller has no opinion; the provider uses its default
///   for the model in question (some models default to thinking on,
///   some to off).
/// - `Some(Thinking { enabled: false, .. })` — explicitly disable on
///   a model that would otherwise think.
/// - `Some(Thinking { enabled: true, .. })` — explicitly enable on a
///   model that would otherwise stay silent.
///
/// Providers that don't support thinking ignore the field entirely —
/// the resulting [`Response::thinking`] will simply be `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thinking {
    /// Whether to request a thinking trace.
    pub enabled: bool,
    /// Optional effort level. See [`ReasoningEffort`].
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// A completion request.
///
/// Provider and profile resolution is the caller's job; this struct
/// only carries the wire payload shape.
#[derive(Debug, Clone)]
pub struct Request {
    /// Conversation so far, in chronological order.
    pub messages: Vec<Message>,
    /// Tool schemas advertised to the model. Empty means "do not
    /// advertise tools"; providers that require the field to be absent
    /// (rather than an empty array) handle that on serialize.
    pub tools: Vec<Tool>,
    /// Model identifier sent in the request body
    /// (for example `"mistral-small-latest"`).
    pub model_name: String,
    /// Upper bound on completion tokens. `None` is serialized by
    /// omission so the provider API applies its own default.
    pub max_tokens: Option<u32>,
    /// Sampling temperature. `None` is serialized by omission so the
    /// provider API applies its own default.
    pub temperature: Option<f32>,
    /// Per-request HTTP timeout in seconds. `None` disables the local
    /// timeout; the request then runs until the remote closes.
    pub timeout_secs: Option<u64>,
    /// Thinking-mode configuration. `None` means "leave the field
    /// unset and let the provider's per-model default apply".
    pub thinking: Option<Thinking>,
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
    /// Chain-of-thought trace, when the provider emitted one and the
    /// caller asked for it via [`Request::thinking`]. `None` for
    /// providers that don't support thinking, for non-thinking calls,
    /// and for thinking calls that simply produced no reasoning.
    pub thinking: Option<String>,
}

/// One incremental chunk in a streaming response.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Incremental text fragment; `None` on chunks that carry only
    /// finish metadata.
    pub content_delta: Option<String>,
    /// Incremental thinking-trace fragment from a reasoning-capable
    /// provider. Wire-mapped from `delta.reasoning_content` for
    /// `DeepSeek`; always `None` for providers without thinking.
    pub thinking_delta: Option<String>,
    /// Per-tool-call partial updates, if present in this chunk.
    pub tool_call_deltas: Option<Vec<ToolCallDelta>>,
    /// Populated only on the final chunk.
    pub finish_reason: Option<FinishReason>,
    /// Populated only on the final chunk; some providers omit entirely.
    pub usage: Option<Usage>,
}

/// A boxed stream of [`StreamChunk`] items yielded by the client's
/// streaming entry point. Alias for [`futures::stream::BoxStream`].
pub type ResponseStream = BoxStream<'static, Result<StreamChunk>>;

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
