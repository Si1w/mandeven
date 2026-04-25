//! LLM client abstraction and per-provider implementations.
//!
//! Consumers talk to models through the [`BaseLLMClient`] trait, which
//! exposes both one-shot [`BaseLLMClient::complete`] and streaming
//! [`BaseLLMClient::stream`]. Individual providers are self-contained
//! under [`providers`]; each provider owns its own HTTP, authentication,
//! and wire-format logic.

pub mod client;
pub mod error;
pub mod providers;
pub mod types;

mod utils;

pub use client::BaseLLMClient;
pub use error::{Error, Result};
pub use types::{
    FinishReason, Message, ReasoningEffort, Request, Response, ResponseStream, Role, StreamChunk,
    Thinking, Tool, ToolCall, ToolCallDelta, Usage,
};
