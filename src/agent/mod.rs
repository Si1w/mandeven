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

pub mod commands;
pub mod error;
pub mod types;

pub use error::{Error, Result};
pub use types::{CallOutcome, Iteration};

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use uuid::Uuid;

use tokio::sync::mpsc;

use crate::bus::{
    ChannelID, InboundPayload, OutboundMessage, OutboundPayload, OutboundSender, SessionID,
};
use crate::command::{CommandOutcome, Router};
use crate::config::{AgentConfig, AppConfig, LLMProfile};
use crate::gateway::{ActiveSessions, DispatchReceiver, InboundDispatch};
use crate::heartbeat::{HeartbeatEngine, HeartbeatTick};
use crate::llm::{
    self, BaseLLMClient, FinishReason, Message, Request, ResponseStream, ToolCall, Usage,
};
use crate::session;
use crate::tools;
use crate::tools::heartbeat::{
    HEARTBEAT_DECIDE_TOOL_NAME, HeartbeatDecideArgs, heartbeat_decide_tool,
};

use self::commands::AgentCommandCtx;

/// System prompt used to generate a short session title from the first
/// user message. This lives inline for now and will migrate to the
/// AGENTS.md-style prompt layer when that lands.
const TITLE_SYSTEM_PROMPT: &str = "Generate a short, descriptive title (max 8 words) for a conversation \
     starting with the following user message. Reply with only the title, \
     no quotes or punctuation.";

/// Upper bound on completion tokens for title generation.
const TITLE_MAX_TOKENS: u32 = 32;

/// Character cap on the fallback title derived from the user's first
/// message when [`Agent::generate_title`] fails or returns empty.
const FALLBACK_TITLE_MAX_CHARS: usize = 40;

/// Channel that heartbeat phase-2 iterations target. Hard-coded
/// because `tui` is the only registered channel today.
//
// TODO(target-routing): switch to the per-tick target resolved from
// `HeartbeatConfig.target` once that field lands. See the
// `target-routing` TODO on `crate::heartbeat::HeartbeatConfig`.
const HEARTBEAT_TARGET_CHANNEL: &str = "tui";

/// System prompt for heartbeat phase-1. Constrains the model to a
/// single tool call so the answer is structured rather than free
/// text.
const HEARTBEAT_DECIDE_SYSTEM_PROMPT: &str = "You are the heartbeat decision step. \
    Read the heartbeat checklist provided and call the heartbeat_decide tool exactly once. \
    Use action=\"skip\" when nothing in the checklist needs attention right now. \
    Use action=\"run\" with a concise one-or-two-sentence summary in `tasks` when at \
    least one item should be acted on now.";

/// Token cap for phase-1. Phase-1 only ever emits a tool call payload
/// (skip/run + a short tasks string), so a tight cap is enough.
const HEARTBEAT_DECIDE_MAX_TOKENS: u32 = 256;

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
    inbox: DispatchReceiver,
    out: OutboundSender,
    config: AgentConfig,
    /// Global HTTP timeout, cached from
    /// [`crate::config::LLMConfig::timeout_secs`] so every iteration
    /// builds its [`Request`] without re-reading the config.
    timeout_secs: Option<u64>,
    /// Agent-level command router. Unknown commands reach the agent
    /// after traversing the channel router and the gateway router;
    /// the agent is the final fallback.
    commands: Router<AgentCommandCtx>,
    /// Heartbeat control handle, present iff the engine was wired in.
    /// Cloned into [`AgentCommandCtx`] so `/heartbeat` subcommands can
    /// pause / resume / set the interval.
    heartbeat: Option<Arc<HeartbeatEngine>>,
    /// Receiver paired with the heartbeat engine. Raced against
    /// `inbox` in [`Agent::run`].
    heartbeat_rx: Option<mpsc::Receiver<HeartbeatTick>>,
    /// Live view of the gateway's per-channel session bindings.
    /// Heartbeat ticks read this to land in the user's main session
    /// rather than running isolated; written only by the gateway.
    active_sessions: ActiveSessions,
}

/// Options for the optional heartbeat wiring. Constructed by
/// `main.rs` and threaded into [`Agent::new`].
pub struct HeartbeatWiring {
    /// Control handle. Stored on the agent and cloned into
    /// [`AgentCommandCtx`].
    pub engine: Arc<HeartbeatEngine>,
    /// Tick stream from the engine, raced against the dispatch
    /// queue in the main loop.
    pub rx: mpsc::Receiver<HeartbeatTick>,
}

/// Single iteration of the agent's `select!` loop. Names what was
/// chosen so [`Agent::run`]'s `match` reads as a state machine.
enum Event {
    /// Inbound dispatch arrived from the gateway. `None` means the
    /// dispatch queue closed (clean shutdown).
    Dispatch(Option<InboundDispatch>),
    /// Heartbeat tick fired. `None` means the engine dropped its
    /// sender — the branch is dynamically disabled afterwards.
    Tick(Option<HeartbeatTick>),
}

impl Agent {
    /// Construct an agent wired to the LLM provider selected by
    /// `cfg.llm.default`, using the caller-supplied tool registry.
    ///
    /// Callers decide which tools are available — pass
    /// [`tools::Registry::new`] for an empty one or use
    /// [`tools::register_builtins`] to install the default set.
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
        tools: tools::Registry,
        inbox: DispatchReceiver,
        out: OutboundSender,
        active_sessions: ActiveSessions,
        heartbeat: Option<HeartbeatWiring>,
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

        // Agent-level command router. Routing / session-level
        // commands (`/new`, `/list`, `/load`) live in the gateway;
        // commands here mutate agent-internal state.
        let mut agent_commands = Router::<AgentCommandCtx>::new();
        agent_commands.register(Arc::new(commands::Heartbeat));

        let (heartbeat_handle, heartbeat_rx) = match heartbeat {
            Some(HeartbeatWiring { engine, rx }) => (Some(engine), Some(rx)),
            None => (None, None),
        };

        Ok(Self {
            profile,
            client,
            sessions,
            tools,
            inbox,
            out,
            config: cfg.agent.clone(),
            timeout_secs: cfg.llm.timeout_secs,
            commands: agent_commands,
            heartbeat: heartbeat_handle,
            heartbeat_rx,
            active_sessions,
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
        loop {
            // Split mut borrows so `tokio::select!` can race two
            // queues without a whole-self conflict. The selected
            // branch's `await` ends before any `&self` method call
            // below (NLL releases `inbox` / `heartbeat_rx` borrows),
            // so subsequent calls into `self.handle_*` are clean.
            let inbox = &mut self.inbox;
            let event = match self.heartbeat_rx.as_mut() {
                Some(hb) => tokio::select! {
                    biased;
                    msg = inbox.recv() => Event::Dispatch(msg),
                    tick = hb.recv() => Event::Tick(tick),
                },
                None => Event::Dispatch(inbox.recv().await),
            };

            match event {
                Event::Dispatch(None) => return Ok(()),
                Event::Dispatch(Some(msg)) => {
                    if !self.handle_dispatch(msg).await? {
                        return Ok(());
                    }
                }
                Event::Tick(None) => {
                    // Engine dropped its sender — disable the branch.
                    self.heartbeat_rx = None;
                }
                Event::Tick(Some(tick)) => self.handle_tick(tick).await?,
            }
        }
    }

    /// Process one inbound dispatch. Returns `Ok(true)` to continue
    /// looping, `Ok(false)` when the outbound bus closed and the
    /// caller should exit.
    async fn handle_dispatch(&self, msg: InboundDispatch) -> Result<bool> {
        let InboundDispatch {
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
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            InboundPayload::Command(body) => {
                // Only failure mode is outbound bus closed → tell the
                // caller to exit.
                Ok(self.dispatch_command(channel, session, &body).await.is_ok())
            }
        }
    }

    /// Handle a heartbeat tick.
    ///
    /// Resolves the user's main session via `active_sessions[tui]`,
    /// runs a phase-1 decide call (gated to the `heartbeat_decide`
    /// tool), and on `run` continues into a full phase-2 iteration
    /// whose outbound stream goes back to the `tui` channel. Skips
    /// silently when:
    ///
    /// - the user has no active main session yet (no `/new` or first
    ///   message), so there is no place to land the heartbeat output
    ///   without spinning up an isolated session;
    /// - phase-1 returns `skip` (or any malformed answer);
    /// - phase-2 errors — the error is rendered as an
    ///   [`OutboundPayload::Error`] on the same target channel.
    async fn handle_tick(&self, tick: HeartbeatTick) -> Result<()> {
        let target = ChannelID::new(HEARTBEAT_TARGET_CHANNEL);
        let session = {
            let map = self.active_sessions.lock().await;
            map.get(&target).cloned()
        };
        let Some(session) = session else {
            return Ok(());
        };

        let prompt = match self.heartbeat_decide(&tick).await {
            Ok(HeartbeatDecision::Run { prompt }) => prompt,
            Ok(HeartbeatDecision::Skip) => return Ok(()),
            Err(err) => {
                eprintln!("[heartbeat] phase-1 failed: {err}");
                return Ok(());
            }
        };

        let iter = Iteration {
            session: session.clone(),
            channel: target.clone(),
        };
        if let Err(err) = self.iteration(&iter, prompt).await {
            let reply =
                OutboundMessage::new(target, session, OutboundPayload::Error(err.to_string()));
            // Outbound bus closed during error reporting just means
            // the channel layer is gone — same shutdown signal the
            // dispatch path treats as `Ok(false)`. Heartbeat ticks
            // can't propagate that, so drop and move on.
            let _ = self.out.send(reply).await;
        }
        Ok(())
    }

    /// Phase-1: ask the model to call `heartbeat_decide` and report
    /// `skip` or `run`. Any non-conforming response (no tool call,
    /// wrong tool name, malformed JSON, `run` without `tasks`) is
    /// folded into [`HeartbeatDecision::Skip`] so a confused model
    /// errs on the side of staying silent.
    async fn heartbeat_decide(&self, tick: &HeartbeatTick) -> Result<HeartbeatDecision> {
        let request = Request {
            messages: vec![
                Message::System {
                    content: HEARTBEAT_DECIDE_SYSTEM_PROMPT.into(),
                },
                Message::User {
                    content: format!(
                        "Current time: {}\n\nHEARTBEAT.md contents:\n\n{}",
                        tick.at, tick.content
                    ),
                },
            ],
            tools: vec![heartbeat_decide_tool()],
            model_name: self.profile.model_name.clone(),
            max_tokens: Some(HEARTBEAT_DECIDE_MAX_TOKENS),
            // Temperature deliberately left unset — see the title
            // generation comment for the reasoning.
            temperature: None,
            timeout_secs: self.timeout_secs,
        };

        let response = self.client.complete(request).await?;
        let Some(call) = response
            .tool_calls
            .into_iter()
            .flatten()
            .find(|c| c.name == HEARTBEAT_DECIDE_TOOL_NAME)
        else {
            return Ok(HeartbeatDecision::Skip);
        };

        let args: HeartbeatDecideArgs = serde_json::from_str(&call.arguments).unwrap_or_default();
        match args.action.as_str() {
            "run" if !args.tasks.trim().is_empty() => {
                Ok(HeartbeatDecision::Run { prompt: args.tasks })
            }
            _ => Ok(HeartbeatDecision::Skip),
        }
    }

    /// Dispatch one forwarded slash command through the agent-level
    /// router and send a reply (when applicable) to the originating
    /// channel. Named to mirror `CliChannel::dispatch_command` — same
    /// role at a different layer. The only failure path is the
    /// outbound bus being closed, in which case the caller should
    /// exit the main loop.
    async fn dispatch_command(
        &self,
        channel: ChannelID,
        session: SessionID,
        body: &str,
    ) -> Result<()> {
        let ctx = AgentCommandCtx {
            channel: channel.clone(),
            session: session.clone(),
            heartbeat: self.heartbeat.clone(),
        };
        let outcome = self.commands.dispatch(body, &ctx).await;

        let payload = match outcome {
            Some(CommandOutcome::Handled) => return Ok(()),
            Some(CommandOutcome::Feedback(msg)) => OutboundPayload::Notice(msg),
            Some(CommandOutcome::Exit) => {
                // Exit has no meaning at the agent layer (agent can't
                // shut down a channel). A command that returns it was
                // registered in the wrong router — log and ignore.
                eprintln!("[agent] command {body:?} returned Exit at agent layer; ignoring");
                return Ok(());
            }
            None => {
                let name = body.split_whitespace().next().unwrap_or(body);
                OutboundPayload::Error(format!("unknown command: /{name}"))
            }
        };

        let reply = OutboundMessage::new(channel, session, payload);
        self.out.send(reply).await?;
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
        self.sessions
            .create(&iter.session, title, iter.channel.clone())
            .await?;
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
        // advertised, tighter token budget. Temperature is left to
        // the provider — not every API honors it (DeepSeek's
        // thinking mode silently drops it, for instance), and a
        // bare title is short enough that sampling jitter doesn't
        // matter.
        request.tools = Vec::new();
        request.max_tokens = Some(TITLE_MAX_TOKENS);
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
    ///
    /// `timeout_secs` is a transport-level concern and therefore pulled
    /// from the shared [`crate::config::LLMConfig::timeout_secs`] rather
    /// than the per-profile block.
    fn build_request(&self, messages: Vec<Message>) -> Request {
        Request {
            messages,
            tools: self.tools.schemas(),
            model_name: self.profile.model_name.clone(),
            max_tokens: self.profile.max_tokens,
            temperature: self.profile.temperature,
            timeout_secs: self.timeout_secs,
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

/// Outcome of heartbeat phase-1.
enum HeartbeatDecision {
    /// Model said "nothing to do" (or returned a malformed answer
    /// that the agent folds into the same branch — better silent
    /// than wrong).
    Skip,
    /// Model said "run" and produced a `tasks` summary; that summary
    /// becomes the phase-2 user message.
    Run { prompt: String },
}
