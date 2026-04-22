//! Agent — ties LLM, session, bus, and tools into an iteration-based
//! loop.
//!
//! The outer [`Agent::run`] loop consumes [`crate::bus::InboundMessage`]s
//! and dispatches each into [`Agent::iteration`], forwarding per-iteration
//! failures back to the source channel as
//! [`crate::bus::OutboundPayload::Error`] without stopping the loop.
//!
//! An iteration composes the capabilities exposed by the domain modules:
//!
//! - [`crate::llm`] — LLM dialing (streaming + non-streaming)
//! - [`crate::session`] — persistent conversation memory
//! - [`crate::bus`] — inbound / outbound message transport
//! - [`crate::tools`] — tool registration and dispatch

pub mod error;
pub mod types;

pub use error::{Error, Result};
pub use types::{CallOutcome, Iteration};

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use uuid::Uuid;

use crate::bus::{
    InboundMessage, InboundPayload, InboundReceiver, OutboundMessage, OutboundPayload,
    OutboundSender, SessionID,
};
use crate::config::{AgentConfig, AppConfig, LLMProfile};
use crate::llm::{
    self, BaseLLMClient, FinishReason, Message, Request, ResponseStream, ToolCall, Usage,
};
use crate::session;
use crate::tools;

/// System prompt used to generate a short session title from the first
/// user message. This lives inline for now and will migrate to the
/// AGENTS.md-style prompt layer when that lands.
const TITLE_SYSTEM_PROMPT: &str = "Generate a short, descriptive title (max 8 words) for a conversation \
     starting with the following user message. Reply with only the title, \
     no quotes or punctuation.";

/// Upper bound on completion tokens for title generation.
const TITLE_MAX_TOKENS: u32 = 32;

/// Sampling temperature for title generation. Lower than chat defaults
/// because titles want determinism.
const TITLE_TEMPERATURE: f32 = 0.3;

/// Character cap on the fallback title derived from the user's first
/// message when [`Agent::generate_title`] fails or returns empty.
const FALLBACK_TITLE_MAX_CHARS: usize = 40;

/// Conversation agent.
///
/// Holds the domain-module handles the iteration loop orchestrates. No
/// additional wrapping layer — `agent` composes domain capabilities
/// directly.
pub struct Agent {
    profile: LLMProfile,
    client: Arc<dyn BaseLLMClient>,
    sessions: Arc<session::Manager>,
    tools: tools::Registry,
    inbox: InboundReceiver,
    out: OutboundSender,
    config: AgentConfig,
}

impl Agent {
    /// Construct an agent wired to the LLM provider selected by
    /// `cfg.llm.default`.
    ///
    /// # Errors
    ///
    /// - [`Error::MalformedProfileId`] when `llm.default` is not of
    ///   the form `"provider/model"`.
    /// - [`Error::ProfileNotFound`] when the referenced profile is
    ///   absent from the config catalog.
    /// - [`Error::UnknownProvider`] when the provider is not
    ///   registered in [`crate::llm::providers`].
    pub fn new(
        cfg: &AppConfig,
        sessions: Arc<session::Manager>,
        inbox: InboundReceiver,
        out: OutboundSender,
    ) -> Result<Self> {
        let (provider_name, model_name) = cfg
            .llm
            .default
            .split_once('/')
            .ok_or_else(|| Error::MalformedProfileId(cfg.llm.default.clone()))?;

        let profile = cfg
            .llm
            .providers
            .get(provider_name)
            .and_then(|models| models.get(model_name))
            .ok_or_else(|| Error::ProfileNotFound {
                provider: provider_name.to_string(),
                model: model_name.to_string(),
            })?
            .clone();

        let client = llm::providers::client_for(provider_name)
            .ok_or_else(|| Error::UnknownProvider(provider_name.to_string()))?;

        Ok(Self {
            profile,
            client,
            sessions,
            tools: tools::Registry::new(),
            inbox,
            out,
            config: cfg.agent.clone(),
        })
    }

    /// Drive the main loop until `inbox` closes.
    ///
    /// Per-iteration errors are turned into
    /// [`OutboundPayload::Error`] and sent back to the originating
    /// channel; the loop continues. The loop only exits when `inbox`
    /// closes or the outbound bus is unable to receive further errors.
    ///
    /// # Errors
    ///
    /// Returns `Ok(())` on clean shutdown. This method currently never
    /// produces an error; the `Result` signature is reserved for
    /// future shutdown-time failures.
    pub async fn run(mut self) -> Result<()> {
        while let Some(msg) = self.inbox.recv().await {
            let InboundMessage {
                channel,
                session,
                payload,
                ..
            } = msg;
            match payload {
                InboundPayload::UserInput(text) => {
                    let iter = Iteration {
                        session: session.clone(),
                        channel: channel.clone(),
                    };
                    if let Err(err) = self.iteration(&iter, text).await {
                        let reply = OutboundMessage::new(
                            channel,
                            session,
                            OutboundPayload::Error(err.to_string()),
                        );
                        if self.out.send(reply).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Execute one conversation iteration — from a user message to the
    /// persisted assistant reply, covering any number of LLM↔tool
    /// calls.
    async fn iteration(&self, iter: &Iteration, user_text: String) -> Result<()> {
        self.ensure_session(iter, &user_text).await?;
        self.sessions
            .append(&iter.session, Message::User { content: user_text })
            .await?;

        let mut i: u8 = 0;
        loop {
            if let Some(cap) = self.config.max_iterations
                && i >= cap
            {
                return Err(Error::MaxIterationsExceeded(cap));
            }

            let messages = self.load_history(&iter.session).await?;
            let outcome = self.call(iter, messages).await?;
            let CallOutcome {
                content,
                tool_calls,
                ..
            } = outcome;

            self.sessions
                .append(
                    &iter.session,
                    Message::Assistant {
                        content: (!content.is_empty()).then_some(content),
                        tool_calls: tool_calls.clone(),
                    },
                )
                .await?;

            let Some(calls) = tool_calls else {
                return Ok(());
            };
            if calls.is_empty() {
                return Ok(());
            }

            for tool_msg in self.tools.dispatch(calls).await {
                self.sessions.append(&iter.session, tool_msg).await?;
            }

            i = i.saturating_add(1);
        }
    }

    /// Issue one LLM call within an iteration and aggregate the
    /// resulting stream.
    async fn call(&self, iter: &Iteration, messages: Vec<Message>) -> Result<CallOutcome> {
        let request = self.build_request(messages);
        let stream = self.client.stream(request).await?;
        self.stream_to_channel(iter, stream).await
    }

    /// Consume a [`ResponseStream`], forwarding text chunks to the bus
    /// as [`OutboundPayload::ReplyDelta`] and emitting
    /// [`OutboundPayload::ReplyEnd`] once the stream terminates.
    /// Returns the aggregated [`CallOutcome`].
    async fn stream_to_channel(
        &self,
        iter: &Iteration,
        mut src: ResponseStream,
    ) -> Result<CallOutcome> {
        let stream_id = Uuid::now_v7();
        let mut content = String::new();
        let mut partial: HashMap<u32, PartialToolCall> = HashMap::new();
        let mut finish_reason: Option<FinishReason> = None;
        let mut usage: Option<Usage> = None;

        while let Some(chunk) = src.next().await {
            let chunk = chunk?;

            if let Some(delta) = chunk.content_delta {
                content.push_str(&delta);
                let msg = OutboundMessage::new(
                    iter.channel.clone(),
                    iter.session.clone(),
                    OutboundPayload::ReplyDelta { stream_id, delta },
                );
                self.out.send(msg).await?;
            }

            if let Some(deltas) = chunk.tool_call_deltas {
                for d in deltas {
                    let entry = partial.entry(d.index).or_default();
                    if d.id.is_some() {
                        entry.id = d.id;
                    }
                    if d.name.is_some() {
                        entry.name = d.name;
                    }
                    if let Some(a) = d.arguments {
                        entry.arguments.push_str(&a);
                    }
                }
            }

            if chunk.finish_reason.is_some() {
                finish_reason = chunk.finish_reason;
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }

        let end = OutboundMessage::new(
            iter.channel.clone(),
            iter.session.clone(),
            OutboundPayload::ReplyEnd { stream_id },
        );
        self.out.send(end).await?;

        Ok(CallOutcome {
            content,
            tool_calls: materialize_tool_calls(partial)?,
            finish_reason: finish_reason.unwrap_or(FinishReason::Stop),
            usage,
        })
    }

    /// Create a session on first encounter, generating a short title
    /// from the user's opening message. If title generation fails or
    /// returns empty, falls back to a truncated prefix of the input.
    async fn ensure_session(&self, iter: &Iteration, first_text: &str) -> Result<()> {
        if self.sessions.metadata(&iter.session).await?.is_some() {
            return Ok(());
        }
        let title = match self.generate_title(first_text).await {
            Ok(t) if !t.is_empty() => t,
            _ => fallback_title(first_text),
        };
        self.sessions.create(&iter.session, title).await?;
        Ok(())
    }

    /// Generate a short session title from the first user message
    /// using a non-streaming completion.
    async fn generate_title(&self, user_input: &str) -> Result<String> {
        let mut request = self.build_request(vec![
            Message::System {
                content: TITLE_SYSTEM_PROMPT.into(),
            },
            Message::User {
                content: user_input.into(),
            },
        ]);
        // Title generation overrides the profile defaults: no tools
        // advertised, tighter token budget, lower temperature.
        request.tools = Vec::new();
        request.max_tokens = TITLE_MAX_TOKENS;
        request.temperature = TITLE_TEMPERATURE;
        let response = self.client.complete(request).await?;
        Ok(response
            .content
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .to_string())
    }

    /// Project the session's record stream into the flat
    /// [`Message`] sequence the LLM request consumes.
    async fn load_history(&self, session: &SessionID) -> Result<Vec<Message>> {
        let records = self.sessions.load(session).await?;
        Ok(records.into_iter().map(|r| r.message).collect())
    }

    /// Fill a fresh [`Request`] from the active profile plus the given
    /// message history and the registry's current tool schemas.
    fn build_request(&self, messages: Vec<Message>) -> Request {
        Request {
            messages,
            tools: self.tools.schemas(),
            model_name: self.profile.model_name.clone(),
            max_tokens: self.profile.max_tokens,
            temperature: self.profile.temperature,
            timeout_secs: self.profile.timeout_secs,
        }
    }
}

/// Accumulates streaming fragments of one tool call until the stream
/// terminates. `id` and `name` are populated once (from the first
/// fragment that carries them); `arguments` concatenates across every
/// fragment for the same index.
#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Turn accumulated per-index fragments into an ordered batch of
/// [`ToolCall`]s. An empty input yields `None`, signalling "no tool
/// calls in this response".
fn materialize_tool_calls(partial: HashMap<u32, PartialToolCall>) -> Result<Option<Vec<ToolCall>>> {
    if partial.is_empty() {
        return Ok(None);
    }
    let mut entries: Vec<_> = partial.into_iter().collect();
    entries.sort_by_key(|(i, _)| *i);
    let calls = entries
        .into_iter()
        .map(|(idx, p)| {
            Ok(ToolCall {
                id: p.id.ok_or_else(|| {
                    Error::MalformedStream(format!("tool_call[{idx}] missing id"))
                })?,
                name: p.name.ok_or_else(|| {
                    Error::MalformedStream(format!("tool_call[{idx}] missing name"))
                })?,
                arguments: p.arguments,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(calls))
}

/// Truncate the user's opening message to produce a human-readable
/// placeholder title when LLM-based generation is unavailable.
fn fallback_title(text: &str) -> String {
    let truncated: String = text.chars().take(FALLBACK_TITLE_MAX_CHARS).collect();
    if text.chars().count() > FALLBACK_TITLE_MAX_CHARS {
        format!("{truncated}…")
    } else {
        truncated
    }
}
