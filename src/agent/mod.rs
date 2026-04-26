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

pub mod command;
pub mod compact;
pub mod error;
pub mod types;

pub use error::{Error, Result};
pub use types::{CallOutcome, Iteration};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use uuid::Uuid;

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

use self::compact::{CompactConfig, CompactState};
use crate::bus::{
    ChannelID, InboundPayload, OutboundMessage, OutboundPayload, OutboundSender, SessionID,
};
use crate::command::{CommandOutcome, Router};
use crate::config::{AgentConfig, AppConfig, LLMProfile};
use crate::cron;
use crate::gateway::{ActiveSessions, DispatchReceiver, InboundDispatch};
use crate::heartbeat::{HeartbeatEngine, HeartbeatTick};
use crate::llm::{
    self, BaseLLMClient, CompactTrigger, FinishReason, Message, Request, ResponseStream, Thinking,
    ToolCall, Usage,
};
use crate::prompt::{PromptContext, PromptEngine};
use crate::session;
use crate::tools;
use crate::tools::heartbeat::{
    HEARTBEAT_DECIDE_TOOL_NAME, HeartbeatDecideArgs, heartbeat_decide_tool,
};

use self::command::{
    AgentCommandCtx, CompactCmdMatch, format_compact_report, parse_compact_command,
};

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
    command: Router<AgentCommandCtx>,
    /// Heartbeat control handle, present iff the engine was wired in.
    /// Cloned into [`AgentCommandCtx`] so `/heartbeat` subcommands can
    /// pause / resume / set the interval.
    heartbeat: Option<Arc<HeartbeatEngine>>,
    /// Receiver paired with the heartbeat engine. Raced against
    /// `inbox` in [`Agent::run`].
    heartbeat_rx: Option<mpsc::Receiver<HeartbeatTick>>,
    /// Cron control handle, present iff the engine was wired in.
    /// Cloned into [`AgentCommandCtx`] so `/cron` subcommands can
    /// list, trigger, or pause individual jobs.
    cron: Option<Arc<cron::CronEngine>>,
    /// Receiver paired with the cron engine. Raced against `inbox`
    /// (and the heartbeat receiver, when present) in [`Agent::run`].
    cron_rx: Option<mpsc::Receiver<cron::CronTick>>,
    /// Live view of the gateway's per-channel session bindings.
    /// Heartbeat ticks read this to land in the user's main session
    /// rather than running isolated; written only by the gateway.
    active_sessions: ActiveSessions,
    /// Window-relative compact thresholds. Cloned from the config
    /// so the agent doesn't refer back to the full `AppConfig` for
    /// every iteration.
    compact_config: CompactConfig,
    /// Mutable compact state — currently just the circuit-breaker
    /// counter. `Mutex` because both the auto-trigger path
    /// ([`Agent::iteration`]) and the manual `/compact` command
    /// (`dispatch_command`) bump it from `&self` async contexts.
    compact_state: Arc<AsyncMutex<CompactState>>,
    /// Prompt assembly engine. Owns `AGENTS.md` plus the section
    /// cache; every call site (`generate_title`, `heartbeat_decide`,
    /// the compact pipeline, the iteration system prompt) goes
    /// through it so future per-task prompt changes only touch one
    /// module.
    prompt: Arc<PromptEngine>,
    /// Process launch directory captured once in `main`. Surfaces in
    /// the per-call `PromptContext.cwd` for `iteration_system`.
    /// Mandeven keeps this stable for the lifetime of the run — the
    /// agent never `cd`s — matching Claude Code's `getOriginalCwd`.
    cwd: PathBuf,
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

/// Options for the optional cron wiring. Same shape as
/// [`HeartbeatWiring`] — `main.rs` constructs the engine and threads
/// these handles into [`Agent::new`].
pub struct CronWiring {
    /// Control handle. Stored on the agent and cloned into
    /// [`AgentCommandCtx`].
    pub engine: Arc<cron::CronEngine>,
    /// Tick stream from the engine.
    pub rx: mpsc::Receiver<cron::CronTick>,
}

/// Single iteration of the agent's `select!` loop. Names what was
/// chosen so [`Agent::run`]'s `match` reads as a state machine.
enum Event {
    /// Inbound dispatch arrived from the gateway. `None` means the
    /// dispatch queue closed (clean shutdown).
    Dispatch(Option<InboundDispatch>),
    /// Heartbeat tick fired. `None` means the engine dropped its
    /// sender — the branch is dynamically disabled afterwards.
    HeartbeatTick(Option<HeartbeatTick>),
    /// Cron tick fired. `None` semantics match the heartbeat arm.
    CronTick(Option<cron::CronTick>),
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
    // The arguments don't cluster naturally — bundling them into a
    // `Wirings`-style struct just for clippy would obscure that this
    // is an internal constructor called once from `main.rs`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &AppConfig,
        sessions: Arc<session::Manager>,
        tools: tools::Registry,
        inbox: DispatchReceiver,
        out: OutboundSender,
        active_sessions: ActiveSessions,
        heartbeat: Option<HeartbeatWiring>,
        cron: Option<CronWiring>,
        prompt: Arc<PromptEngine>,
        cwd: PathBuf,
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
        let mut agent_command = Router::<AgentCommandCtx>::new();
        agent_command.register(Arc::new(command::Heartbeat));
        agent_command.register(Arc::new(command::Cron));

        let (heartbeat_handle, heartbeat_rx) = match heartbeat {
            Some(HeartbeatWiring { engine, rx }) => (Some(engine), Some(rx)),
            None => (None, None),
        };
        let (cron_handle, cron_rx) = match cron {
            Some(CronWiring { engine, rx }) => (Some(engine), Some(rx)),
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
            command: agent_command,
            heartbeat: heartbeat_handle,
            heartbeat_rx,
            cron: cron_handle,
            cron_rx,
            active_sessions,
            compact_config: cfg.agent.compact.clone(),
            compact_state: Arc::new(AsyncMutex::new(CompactState::new())),
            prompt,
            cwd,
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
            // Split mut borrows so `tokio::select!` can race three
            // queues without a whole-self conflict. The selected
            // branch's `await` ends before any `&self` method call
            // below (NLL releases the field borrows), so subsequent
            // calls into `self.handle_*` are clean.
            let inbox = &mut self.inbox;
            let event = match (self.heartbeat_rx.as_mut(), self.cron_rx.as_mut()) {
                (Some(hb), Some(cr)) => tokio::select! {
                    biased;
                    msg = inbox.recv() => Event::Dispatch(msg),
                    tick = hb.recv() => Event::HeartbeatTick(tick),
                    tick = cr.recv() => Event::CronTick(tick),
                },
                (Some(hb), None) => tokio::select! {
                    biased;
                    msg = inbox.recv() => Event::Dispatch(msg),
                    tick = hb.recv() => Event::HeartbeatTick(tick),
                },
                (None, Some(cr)) => tokio::select! {
                    biased;
                    msg = inbox.recv() => Event::Dispatch(msg),
                    tick = cr.recv() => Event::CronTick(tick),
                },
                (None, None) => Event::Dispatch(inbox.recv().await),
            };

            match event {
                Event::Dispatch(None) => return Ok(()),
                Event::Dispatch(Some(msg)) => {
                    if !self.handle_dispatch(msg).await? {
                        return Ok(());
                    }
                }
                Event::HeartbeatTick(None) => {
                    // Engine dropped its sender — disable the branch.
                    self.heartbeat_rx = None;
                }
                Event::HeartbeatTick(Some(tick)) => self.handle_tick(tick).await?,
                Event::CronTick(None) => {
                    self.cron_rx = None;
                }
                Event::CronTick(Some(tick)) => self.handle_cron_tick(tick).await?,
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

    /// Handle a cron tick.
    ///
    /// Unlike heartbeat there is no phase-1 decide step — the user
    /// already told us when this job should run, so we go straight
    /// into a phase-2 iteration with the job's prompt as the user
    /// message. The outcome is reported back to
    /// [`cron::CronEngine::report_outcome`] so consecutive-error
    /// auto-disable can fire.
    ///
    /// Skipped silently when the `tui` channel has no active session
    /// yet — same reasoning as heartbeat (no place to land output).
    /// The engine still hears about the skip so `last_status` shows
    /// it.
    async fn handle_cron_tick(&self, tick: cron::CronTick) -> Result<()> {
        let target = ChannelID::new(HEARTBEAT_TARGET_CHANNEL);
        let session = {
            let map = self.active_sessions.lock().await;
            map.get(&target).cloned()
        };
        let Some(session) = session else {
            if let Some(engine) = self.cron.as_ref() {
                engine
                    .report_outcome(&tick.job_id, cron::RunStatus::Skipped, None)
                    .await;
            }
            return Ok(());
        };

        let iter = Iteration {
            session: session.clone(),
            channel: target.clone(),
        };
        match self.iteration(&iter, tick.prompt.clone()).await {
            Ok(()) => {
                if let Some(engine) = self.cron.as_ref() {
                    engine
                        .report_outcome(&tick.job_id, cron::RunStatus::Ok, None)
                        .await;
                }
            }
            Err(err) => {
                let err_text = err.to_string();
                if let Some(engine) = self.cron.as_ref() {
                    engine
                        .report_outcome(
                            &tick.job_id,
                            cron::RunStatus::Error,
                            Some(err_text.clone()),
                        )
                        .await;
                }
                let reply = OutboundMessage::new(
                    target,
                    session,
                    OutboundPayload::Error(format!("[cron:{}] {err_text}", tick.job_name)),
                );
                let _ = self.out.send(reply).await;
            }
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
            messages: self
                .prompt
                .heartbeat_decide_messages(&tick.content, tick.at),
            tools: vec![heartbeat_decide_tool()],
            model_name: self.profile.model_name.clone(),
            max_tokens: Some(HEARTBEAT_DECIDE_MAX_TOKENS),
            // Temperature deliberately left unset — see the title
            // generation comment for the reasoning.
            temperature: None,
            timeout_secs: self.timeout_secs,
            // Phase-1 is a structured tool call; the reasoning trace
            // would be discarded anyway. Leave thinking off so even
            // a thinking-capable model returns just the tool call.
            thinking: Some(Thinking {
                enabled: false,
                reasoning_effort: None,
            }),
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
        // `/compact [focus]` is special-cased rather than registered
        // on the router because its execution needs an `&self` LLM
        // call + session write — neither fits the static
        // `Command<AgentCommandCtx>` trait shape (which returns a
        // synchronous `CommandOutcome`).
        match parse_compact_command(body) {
            CompactCmdMatch::Bare => return self.run_compact_command(channel, session, None).await,
            CompactCmdMatch::Focused(focus) => {
                return self
                    .run_compact_command(channel, session, Some(focus))
                    .await;
            }
            CompactCmdMatch::None => {}
        }

        let ctx = AgentCommandCtx {
            channel: channel.clone(),
            session: session.clone(),
            heartbeat: self.heartbeat.clone(),
            cron: self.cron.clone(),
        };
        let outcome = self.command.dispatch(body, &ctx).await;

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

    /// Auto-compact gate. Called by [`Self::iteration`] just before
    /// each LLM call. Returns the (possibly compacted) message list
    /// the call should use. Auto compaction is silent on success per
    /// the agreed UX — a `Notice` is only emitted when the breaker
    /// trips or the summarize call fails outright.
    async fn maybe_auto_compact(
        &self,
        iter: &Iteration,
        messages: Vec<Message>,
    ) -> Result<Vec<Message>> {
        if !compact::should_compact(&messages, &self.profile, &self.compact_config) {
            return Ok(messages);
        }
        let mut state = self.compact_state.lock().await;
        if state.is_circuit_open(&self.compact_config) {
            // Don't drop the breaker by retrying — tell the user
            // once and let the LLM call fall through (it will
            // produce its own context-too-long error if applicable).
            self.send_notice(
                iter,
                "context full and compact circuit breaker open — start a fresh session with /new",
            )
            .await;
            return Ok(messages);
        }

        let summary_system = self.prompt.compact_summary_system(None);
        match compact::compact_messages(
            messages.clone(),
            &self.profile,
            self.client.as_ref(),
            &self.compact_config,
            &mut state,
            CompactTrigger::Auto,
            &summary_system,
            self.timeout_secs,
        )
        .await
        {
            Ok((compacted, _report)) => {
                self.sessions
                    .replace_messages(&iter.session, compacted.clone())
                    .await?;
                // Compaction rewrites the conversation prefix, so any
                // cached `iteration_system` sections are about to be
                // re-emitted into a brand-new context. Drop the cache
                // to mirror Claude Code's `clearSystemPromptSections`
                // behavior — same reasoning, same timing.
                self.prompt.clear_cache();
                Ok(compacted)
            }
            Err(err) => {
                self.send_notice(iter, &format!("auto-compact failed: {err}"))
                    .await;
                // Fall through with the original messages so the
                // user's turn still gets attempted; the LLM may
                // succeed or surface a clearer error.
                Ok(messages)
            }
        }
    }

    /// Manual `/compact [focus]` handler. Reachable only via
    /// [`Self::dispatch_command`]'s special case. Reports the
    /// outcome through a single `Notice` (success) or `Error`
    /// (failure) on the originating channel.
    async fn run_compact_command(
        &self,
        channel: ChannelID,
        session: SessionID,
        focus: Option<String>,
    ) -> Result<()> {
        let messages = self.load_history(&session).await?;
        let summary_system = self.prompt.compact_summary_system(focus.as_deref());
        let mut state = self.compact_state.lock().await;
        let result = compact::compact_messages(
            messages,
            &self.profile,
            self.client.as_ref(),
            &self.compact_config,
            &mut state,
            CompactTrigger::Manual,
            &summary_system,
            self.timeout_secs,
        )
        .await;
        // Drop the lock before any outbound send — `send` can take a
        // while if the channel is congested, no need to hold state
        // across it.
        drop(state);

        let payload = match result {
            Ok((compacted, report)) => {
                self.sessions.replace_messages(&session, compacted).await?;
                // Same reasoning as the auto path in
                // `maybe_auto_compact`: the prefix changed, so the
                // section cache should rebuild.
                self.prompt.clear_cache();
                OutboundPayload::Notice(format_compact_report(&report))
            }
            Err(err) => OutboundPayload::Error(format!("/compact failed: {err}")),
        };
        let reply = OutboundMessage::new(channel, session, payload);
        self.out.send(reply).await?;
        Ok(())
    }

    /// Send an agent-originated `Notice` to the channel that
    /// triggered the current iteration. Failures (outbound bus
    /// closed) are swallowed — the caller has already chosen to
    /// continue, and there is nothing useful to do with the error
    /// at this granularity.
    async fn send_notice(&self, iter: &Iteration, text: &str) {
        let msg = OutboundMessage::new(
            iter.channel.clone(),
            iter.session.clone(),
            OutboundPayload::Notice(text.to_string()),
        );
        let _ = self.out.send(msg).await;
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
            let messages = self.maybe_auto_compact(iter, messages).await?;
            // Prepend the freshly-built iteration system prompt
            // here rather than persisting it: env_info changes every
            // call, AGENTS.md edits should take effect on the next
            // iteration without rewriting history, and Claude Code
            // does the same — the system prompt is rendered into
            // toolUseContext.renderedSystemPrompt per call, never
            // appended to the transcript.
            let messages = self.prepend_iteration_system(messages);
            let outcome = self.call(iter, messages).await?;
            let CallOutcome {
                content,
                thinking,
                tool_calls,
                ..
            } = outcome;

            self.sessions
                .append(
                    &iter.session,
                    Message::Assistant {
                        content: (!content.is_empty()).then_some(content),
                        tool_calls: tool_calls.clone(),
                        reasoning: thinking,
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

    /// Render the iteration system prompt and prepend it to
    /// `messages`. Called from [`Self::iteration`] only — the title
    /// and heartbeat-decide call paths use their own specialized
    /// prompts and must not see the iteration system block.
    fn prepend_iteration_system(&self, mut messages: Vec<Message>) -> Vec<Message> {
        let ctx = PromptContext {
            model_id: &self.profile.model_name,
            cwd: &self.cwd,
        };
        let system = self.prompt.iteration_system(&ctx).into_message();
        let mut out = Vec::with_capacity(messages.len() + 1);
        out.push(system);
        out.append(&mut messages);
        out
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
        let mut thinking = String::new();
        let mut partial: HashMap<u32, PartialToolCall> = HashMap::new();
        let mut finish_reason: Option<FinishReason> = None;
        let mut usage: Option<Usage> = None;

        while let Some(chunk) = src.next().await {
            let chunk = chunk?;

            if let Some(delta) = chunk.thinking_delta {
                thinking.push_str(&delta);
                let msg = OutboundMessage::new(
                    iter.channel.clone(),
                    iter.session.clone(),
                    OutboundPayload::ThinkingDelta { stream_id, delta },
                );
                self.out.send(msg).await?;
            }

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
            thinking: (!thinking.is_empty()).then_some(thinking),
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
        let mut request = self.build_request(self.prompt.title_messages(user_input));
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
        // Translate the per-profile `thinking: Option<bool>` knob
        // into a wire-shape `Option<Thinking>`:
        //
        // - `None`        → leave `extra_body.thinking` unset; the
        //                   provider applies its per-model default
        //                   (DeepSeek's docs: thinking-capable models
        //                   default to `enabled`).
        // - `Some(true)`  → explicit enable.
        // - `Some(false)` → explicit disable on a model that would
        //                   otherwise think (avoids surprise cost).
        let thinking = self.profile.thinking.map(|enabled| Thinking {
            enabled,
            reasoning_effort: None,
        });
        Request {
            messages,
            tools: self.tools.schemas(),
            model_name: self.profile.model_name.clone(),
            max_tokens: self.profile.max_tokens,
            temperature: self.profile.temperature,
            timeout_secs: self.timeout_secs,
            thinking,
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
