//! Agent — ties LLM, session, bus, and tools into an iteration-based
//! loop.
//!
//! The outer [`Agent::run`] loop consumes [`crate::bus::InboundMessage`]s
//! and dispatches each into `Agent::iteration`, forwarding per-iteration
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
pub use types::{CallOutcome, Iteration, SessionScope};

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use futures::StreamExt;
use uuid::Uuid;

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

use self::compact::{CompactConfig, CompactState};
use crate::bus::{
    ChannelID, InboundPayload, OutboundMessage, OutboundPayload, OutboundSender, SessionID,
};
use crate::command::CommandOutcome;
use crate::command::slash::{self, SlashCommand, SwitchCommand};
use crate::config::{AgentConfig, AppConfig, LLMConfig, LLMProfile};
use crate::exec;
use crate::gateway::{ActiveSessions, DispatchReceiver, InboundDispatch};
use crate::hook::{HookEngine, HookEvent};
use crate::llm::{
    self, BaseLLMClient, CompactTrigger, FinishReason, Message, Request, ResponseStream, Thinking,
    ToolCall, Usage,
};
use crate::memory;
use crate::prompt::{PromptContext, PromptEngine};
use crate::session;
use crate::skill::SkillIndex;
use crate::task;
use crate::timer;
use crate::tools;

use self::command::{
    AgentCommandCtx, format_compact_report, run_discord_command, run_wechat_command,
};

/// Upper bound on completion tokens for title generation.
const TITLE_MAX_TOKENS: u32 = 32;

/// Character cap on the fallback title derived from the user's first
/// message when [`Agent::generate_title`] fails or returns empty.
const FALLBACK_TITLE_MAX_CHARS: usize = 40;

/// Channel that receives ambient notifications when a background
/// timer run produces user-visible output.
const DEFAULT_NOTIFY_CHANNEL: &str = "tui";

/// Synthetic channel id persisted on background sessions. It is not
/// registered with the channel manager; silent delivery prevents bus
/// sends to this id.
const CRON_CHANNEL: &str = "cron";

/// Marker a background skill can emit to suppress ambient notices.
const SILENT_MARKER: &str = "[SILENT]";

/// Conversation agent.
///
/// Holds the domain-module handles the iteration loop orchestrates. No
/// additional wrapping layer — `agent` composes domain capabilities
/// directly.
pub struct Agent {
    model: Arc<RwLock<ModelSnapshot>>,
    model_catalog: Arc<ModelCatalog>,
    app_config: Arc<RwLock<AppConfig>>,
    sessions: Arc<session::Manager>,
    cron_sessions: Arc<session::Manager>,
    tools: tools::Registry,
    inbox: DispatchReceiver,
    out: OutboundSender,
    config: AgentConfig,
    /// Global HTTP timeout, cached from
    /// [`crate::config::LLMConfig::timeout_secs`] so every iteration
    /// builds its [`Request`] without re-reading the config.
    timeout_secs: Option<u64>,
    /// Timer scheduler handle, present iff the timer engine was wired
    /// in.
    timer: Option<Arc<timer::TimerEngine>>,
    /// Receiver paired with the timer engine. Timer ticks are the
    /// runtime form of `task + timer` state.
    timer_rx: Option<mpsc::Receiver<timer::TimerTick>>,
    /// Durable `MEMORY.md` manager. The file is injected as transient
    /// user context on each model request and edited through the
    /// `memorize` skill plus normal file tools.
    memory: Arc<memory::Manager>,
    /// Project-local task manager used by explicit `task_run`
    /// execution.
    tasks: Arc<task::Manager>,
    /// Live skill index used by global skill timers to expand timer
    /// ticks into the same body the `/name` slash fallback would send.
    skills: Arc<SkillIndex>,
    /// Machine-readable execution history writer. Timer-triggered
    /// tasks append JSONL events here.
    exec: Arc<exec::Manager>,
    /// Discord adapter control handle, present iff the channel was
    /// registered. Cloned into [`AgentCommandCtx`] so `/discord
    /// allow|deny|list` can mutate the runtime allow list.
    discord: Option<crate::channels::discord::DiscordControl>,
    /// `WeChat` adapter control handle, present iff the channel was
    /// registered.
    wechat: Option<crate::channels::wechat::WechatControl>,
    /// Live view of the gateway's per-identity session bindings.
    /// Background timer runs read this to send a final ambient
    /// notice to the user's current TUI session when there is one.
    /// Written only by the gateway.
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
    /// Prompt assembly engine. Owns `AGENTS.md`, the live skill
    /// index handle, and the section cache; every call site goes
    /// through it so future per-task prompt changes only touch one
    /// module.
    prompt: Arc<PromptEngine>,
    /// Hook engine. Fired at every lifecycle event (`UserPromptSubmit`,
    /// `Pre/PostToolUse`, `SessionStart`, `Stop`, `Pre/PostCompact`).
    /// When `enabled = false` or no `hooks.json` exists, every fire
    /// becomes a no-op. When the file changes, the engine reloads it
    /// on the next fire.
    hook: Arc<HookEngine>,
    /// Process launch directory captured once in `main`. Surfaces in
    /// the per-call `PromptContext.cwd` for `iteration_system` and
    /// in every hook's `MANDEVEN_CWD` env var. Mandeven keeps this
    /// stable for the lifetime of the run — the agent never `cd`s —
    /// matching Claude Code's `getOriginalCwd`.
    cwd: PathBuf,
}

#[derive(Clone)]
struct ModelSnapshot {
    id: String,
    profile: LLMProfile,
    client: Arc<dyn BaseLLMClient>,
}

struct ModelCatalog {
    entries: BTreeMap<String, ModelSnapshot>,
}

impl ModelCatalog {
    fn from_config(cfg: &LLMConfig) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for (provider, models) in &cfg.providers {
            let client = llm::providers::client_for(provider)
                .ok_or_else(|| Error::UnknownProvider(provider.clone()))?;
            for (model, profile) in models {
                let id = format!("{provider}/{model}");
                entries.insert(
                    id.clone(),
                    ModelSnapshot {
                        id,
                        profile: profile.clone(),
                        client: client.clone(),
                    },
                );
            }
        }
        Ok(Self { entries })
    }

    fn get(&self, raw: &str) -> Result<ModelSnapshot> {
        let (provider, model) = parse_profile_id(raw)?;
        self.entries
            .get(raw)
            .cloned()
            .ok_or_else(|| Error::ProfileNotFound {
                provider: provider.to_string(),
                model: model.to_string(),
            })
    }

    fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

fn parse_profile_id(raw: &str) -> Result<(&str, &str)> {
    let (provider, model) = raw
        .split_once('/')
        .ok_or_else(|| Error::MalformedProfileId(raw.to_string()))?;
    if provider.is_empty() || model.is_empty() || model.contains('/') {
        return Err(Error::MalformedProfileId(raw.to_string()));
    }
    Ok((provider, model))
}

/// Options for the timer scheduler.
pub struct TimerWiring {
    /// Scheduler handle.
    pub engine: Arc<timer::TimerEngine>,
    /// Tick stream from the scheduler.
    pub rx: mpsc::Receiver<timer::TimerTick>,
}

/// Options for the optional Discord wiring. Carries only the runtime
/// control handle — Discord has no tick stream, just the
/// allowlist mutator. Threaded into [`Agent::new`] alongside other
/// optional subsystem wiring.
pub struct DiscordWiring {
    /// Allowlist mutator, cloned into [`AgentCommandCtx`].
    pub control: crate::channels::discord::DiscordControl,
}

/// Options for optional `WeChat` wiring.
pub struct WechatWiring {
    /// Runtime control handle cloned into [`AgentCommandCtx`].
    pub control: crate::channels::wechat::WechatControl,
}

/// Single iteration of the agent's `select!` loop. Names what was
/// chosen so [`Agent::run`]'s `match` reads as a state machine.
enum Event {
    /// Inbound dispatch arrived from the gateway. `None` means the
    /// dispatch queue closed (clean shutdown).
    Dispatch(Option<InboundDispatch>),
    /// Timer tick fired. `None` means the engine dropped its sender.
    TimerTick(Option<timer::TimerTick>),
}

async fn recv_enabled<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    rx.as_mut()
        .expect("select branch only enabled when receiver is present")
        .recv()
        .await
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
        cron_sessions: Arc<session::Manager>,
        tools: tools::Registry,
        inbox: DispatchReceiver,
        out: OutboundSender,
        active_sessions: ActiveSessions,
        timer: Option<TimerWiring>,
        memory: Arc<memory::Manager>,
        tasks: Arc<task::Manager>,
        skills: Arc<SkillIndex>,
        exec: Arc<exec::Manager>,
        discord: Option<DiscordWiring>,
        wechat: Option<WechatWiring>,
        prompt: Arc<PromptEngine>,
        hook: Arc<HookEngine>,
        cwd: PathBuf,
    ) -> Result<Self> {
        let model_catalog = Arc::new(ModelCatalog::from_config(&cfg.llm)?);
        let model = Arc::new(RwLock::new(model_catalog.get(&cfg.llm.default)?));

        let (timer_handle, timer_rx) = match timer {
            Some(TimerWiring { engine, rx }) => (Some(engine), Some(rx)),
            None => (None, None),
        };
        let discord_handle = discord.map(|w| w.control);
        let wechat_handle = wechat.map(|w| w.control);

        Ok(Self {
            model,
            model_catalog,
            app_config: Arc::new(RwLock::new(cfg.clone())),
            sessions,
            cron_sessions,
            tools,
            inbox,
            out,
            config: cfg.agent.clone(),
            timeout_secs: cfg.llm.timeout_secs,
            timer: timer_handle,
            timer_rx,
            memory,
            tasks,
            skills,
            exec,
            discord: discord_handle,
            wechat: wechat_handle,
            active_sessions,
            compact_config: cfg.agent.compact.clone(),
            compact_state: Arc::new(AsyncMutex::new(CompactState::new())),
            prompt,
            hook,
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
            // Split mut borrows so `tokio::select!` can race queues
            // without a whole-self conflict. The selected branch's
            // `await` ends before any `&self` method call below (NLL
            // releases the field borrows), so subsequent calls into
            // `self.handle_*` are clean.
            let timer_enabled = self.timer_rx.is_some();
            let inbox = &mut self.inbox;
            let timer_rx = &mut self.timer_rx;
            let event = tokio::select! {
                biased;
                msg = inbox.recv() => Event::Dispatch(msg),
                tick = recv_enabled(timer_rx), if timer_enabled => Event::TimerTick(tick),
            };

            match event {
                Event::Dispatch(None) => return Ok(()),
                Event::Dispatch(Some(msg)) => {
                    if !self.handle_dispatch(msg).await? {
                        return Ok(());
                    }
                }
                Event::TimerTick(None) => {
                    self.timer_rx = None;
                }
                Event::TimerTick(Some(tick)) => self.handle_timer_tick(tick).await?,
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
            session_key,
            payload,
            ..
        } = msg;
        match payload {
            InboundPayload::UserInput(text) => {
                let iter = Iteration::visible_with_identity(
                    session.clone(),
                    channel.clone(),
                    session_key.peer_id,
                    session_key.account_id,
                    session_key.guild_id,
                    None,
                );
                if let Err(err) = self.iteration(&iter, text).await {
                    let reply = OutboundMessage::new(
                        channel.clone(),
                        session.clone(),
                        OutboundPayload::Error(err.to_string()),
                    );
                    if self.out.send(reply).await.is_err() {
                        return Ok(false);
                    }
                }
                if self.send_turn_end(&iter).await.is_err() {
                    return Ok(false);
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

    /// Handle a timer tick.
    ///
    /// Timers are the runtime form of `task + timer` state. The
    /// scheduler has already advanced the timer before emitting this
    /// tick. Task timers always run in the fixed cron bucket; skill
    /// timers choose foreground vs. background through frontmatter
    /// `fork`.
    async fn handle_timer_tick(&self, tick: timer::TimerTick) -> Result<()> {
        if self.timer.is_none() {
            return Ok(());
        }
        let notify = self.active_notify_target().await;
        let timer_id = tick.timer_id;
        match tick.target {
            timer::TimerTarget::Task {
                task_id,
                task_subject,
                prompt,
            } => {
                let timer_title = task_subject.clone();
                let task = exec::TaskExecution {
                    task_id,
                    task_subject: task_subject.clone(),
                    prompt,
                    trigger: exec::ExecTrigger::Timer {
                        timer_id: timer_id.clone(),
                        timer_title: timer_title.clone(),
                    },
                };
                let label = format!("[timer:{timer_title} / task:{task_subject}]");
                match self
                    .run_in_forked_session(
                        format!("timer: {timer_title}"),
                        task.prompt.clone(),
                        Some(timer_id),
                        Some(task),
                    )
                    .await
                {
                    Ok(output) => {
                        self.notify_background_output(notify, &label, &output).await;
                    }
                    Err(err) => {
                        self.notify_background_error(notify, &label, &err).await;
                    }
                }
            }
            timer::TimerTarget::Skill { skill } => {
                return self.handle_skill_timer_tick(timer_id, skill, notify).await;
            }
        }
        Ok(())
    }

    async fn handle_skill_timer_tick(
        &self,
        timer_id: String,
        skill: String,
        notify: Option<(ChannelID, SessionID)>,
    ) -> Result<()> {
        let Some((body, fork)) = self
            .skills
            .get(&skill)
            .map(|skill_def| (skill_def.body.clone(), skill_def.frontmatter.fork))
        else {
            eprintln!("[timer] skill timer {timer_id} references unknown skill /{skill}");
            return Ok(());
        };
        let label = format!("[timer:{timer_id} / skill:{skill}]");
        if fork {
            match self
                .run_in_forked_session(format!("skill timer: {skill}"), body, Some(timer_id), None)
                .await
            {
                Ok(output) => {
                    self.notify_background_output(notify, &label, &output).await;
                }
                Err(err) => {
                    self.notify_background_error(notify, &label, &err).await;
                }
            }
            return Ok(());
        }

        let Some((channel, session)) = notify else {
            eprintln!("[timer] skipped foreground skill timer {label}: no active TUI session");
            return Ok(());
        };
        let iter = Iteration::visible(session.clone(), channel.clone(), None);
        if let Err(err) = self.iteration(&iter, body).await {
            let reply = OutboundMessage::new(
                channel.clone(),
                session,
                OutboundPayload::Error(format!("{label} {err}")),
            );
            let _ = self.out.send(reply).await;
        }
        let _ = self.send_turn_end(&iter).await;
        Ok(())
    }

    async fn active_notify_target(&self) -> Option<(ChannelID, SessionID)> {
        let channel = ChannelID::new(DEFAULT_NOTIFY_CHANNEL);
        let session = {
            let map = self.active_sessions.lock().await;
            map.get(&crate::gateway::SessionKey::channel_only(channel.clone()))
                .cloned()
                .or_else(|| {
                    map.iter()
                        .find(|(key, _)| key.channel == channel)
                        .map(|(_, session)| session.clone())
                })
        }?;
        Some((channel, session))
    }

    /// Execute one input in a fresh silent session under the fixed
    /// cron bucket. Used by scheduled tasks and forked skill timers.
    async fn run_in_forked_session(
        &self,
        title: String,
        input: String,
        timer_id: Option<String>,
        execution: Option<exec::TaskExecution>,
    ) -> Result<String> {
        let session = SessionID::new();
        let channel = ChannelID::new(CRON_CHANNEL);
        let exec_id = if let Some(execution) = execution.as_ref() {
            match self
                .exec
                .start(execution.start(session.clone(), channel.clone()))
                .await
            {
                Ok(exec_id) => Some(exec_id),
                Err(err) => {
                    eprintln!("[exec] failed to start execution log: {err}");
                    None
                }
            }
        } else {
            None
        };
        let iter = Iteration::silent_cron(session.clone(), channel.clone(), exec_id.clone());
        self.cron_sessions.create(&session, title, channel).await?;
        let input = with_timer_context(input, timer_id.as_deref());
        match self.iteration(&iter, input).await {
            Ok(output) => {
                if let Some(exec_id) = exec_id.as_ref() {
                    if let Err(err) = self.exec.final_output(exec_id, output.clone()).await {
                        eprintln!("[exec] failed to write final output: {err}");
                    }
                    if let Err(err) = self
                        .exec
                        .finish(exec_id, exec::ExecStatus::Succeeded, None)
                        .await
                    {
                        eprintln!("[exec] failed to finish execution log: {err}");
                    }
                }
                Ok(output)
            }
            Err(err) => {
                if let Some(exec_id) = exec_id.as_ref()
                    && let Err(log_err) = self
                        .exec
                        .finish(exec_id, exec::ExecStatus::Failed, Some(err.to_string()))
                        .await
                {
                    eprintln!("[exec] failed to mark execution failed: {log_err}");
                }
                Err(err)
            }
        }
    }

    async fn notify_background_output(
        &self,
        notify: Option<(ChannelID, SessionID)>,
        label: &str,
        output: &str,
    ) {
        if !should_notify_background(output) {
            return;
        }
        let Some((channel, session)) = notify else {
            eprintln!("[timer] background output produced with no active TUI target: {label}");
            return;
        };
        let text = format!("{label}\n\n{}", output.trim());
        let msg = OutboundMessage::new(channel, session, OutboundPayload::Notice(text));
        let _ = self.out.send(msg).await;
    }

    async fn notify_background_error(
        &self,
        notify: Option<(ChannelID, SessionID)>,
        label: &str,
        err: &Error,
    ) {
        let text = format!("{label} {err}");
        let Some((channel, session)) = notify else {
            eprintln!("{text}");
            return;
        };
        let msg = OutboundMessage::new(channel, session, OutboundPayload::Error(text));
        let _ = self.out.send(msg).await;
    }

    /// Dispatch one forwarded slash command through the agent layer
    /// and send a reply (when applicable) to the originating
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
        let parsed = match slash::parse(body) {
            Ok(parsed) => parsed,
            Err(err) => {
                let reply = OutboundMessage::new(channel, session, OutboundPayload::Error(err));
                self.out.send(reply).await?;
                return Ok(());
            }
        };

        let parsed = match parsed {
            SlashCommand::Compact { focus } => {
                return self.run_compact_command(channel, session, focus).await;
            }
            other => other,
        };

        let ctx = AgentCommandCtx {
            channel: channel.clone(),
            session: session.clone(),
            discord: self.discord.clone(),
            wechat: self.wechat.clone(),
            out: self.out.clone(),
            app_config: self.app_config.clone(),
        };

        let payload = match parsed {
            SlashCommand::Switch(command) => self.run_switch_command(command),
            SlashCommand::Discord(command) => match run_discord_command(command, &ctx).await {
                CommandOutcome::Completed => return Ok(()),
                CommandOutcome::Feedback(msg) => OutboundPayload::Notice(msg),
                CommandOutcome::Exit => {
                    eprintln!("[agent] command {body:?} returned Exit at agent layer; ignoring");
                    return Ok(());
                }
            },
            SlashCommand::Wechat(command) => match run_wechat_command(command, &ctx).await {
                CommandOutcome::Completed => return Ok(()),
                CommandOutcome::Feedback(msg) => OutboundPayload::Notice(msg),
                CommandOutcome::Exit => {
                    eprintln!("[agent] command {body:?} returned Exit at agent layer; ignoring");
                    return Ok(());
                }
            },
            SlashCommand::External { name, .. } => {
                OutboundPayload::Error(format!("unknown command: /{name}"))
            }
            SlashCommand::Help
            | SlashCommand::Skills
            | SlashCommand::Exit
            | SlashCommand::Quit
            | SlashCommand::New
            | SlashCommand::List
            | SlashCommand::Load { .. }
            | SlashCommand::Compact { .. } => {
                let name = body.split_whitespace().next().unwrap_or(body);
                OutboundPayload::Error(format!("unknown command: /{name}"))
            }
        };

        let reply = OutboundMessage::new(channel, session, payload);
        self.out.send(reply).await?;
        Ok(())
    }

    fn run_switch_command(&self, command: SwitchCommand) -> OutboundPayload {
        match command {
            SwitchCommand::List => OutboundPayload::Notice(self.format_model_list()),
            SwitchCommand::Runtime { profile_id } => self
                .switch_runtime_model(&profile_id)
                .unwrap_or_else(OutboundPayload::Error),
            SwitchCommand::ShowDefault => OutboundPayload::Notice(self.format_default_model()),
            SwitchCommand::SetDefault { profile_id } => self
                .switch_default_model(&profile_id)
                .unwrap_or_else(OutboundPayload::Error),
        }
    }

    fn switch_runtime_model(&self, target: &str) -> std::result::Result<OutboundPayload, String> {
        let snapshot = self.model_catalog.get(target).map_err(|_| {
            format!("unknown model profile: {target}; run /switch to list available profiles")
        })?;
        self.set_model_snapshot(snapshot);
        Ok(OutboundPayload::Notice(format!(
            "switched model to {}",
            self.current_model_label()
        )))
    }

    fn switch_default_model(&self, target: &str) -> std::result::Result<OutboundPayload, String> {
        let snapshot = self.model_catalog.get(target).map_err(|_| {
            format!("unknown model profile: {target}; run /switch to list available profiles")
        })?;

        {
            let mut cfg = self.app_config.write().expect("config lock poisoned");
            let previous = cfg.llm.default.clone();
            cfg.llm.default = target.to_string();
            if let Err(err) = cfg.save() {
                cfg.llm.default = previous;
                return Err(format!("failed to save default model: {err}"));
            }
        }

        self.set_model_snapshot(snapshot);
        Ok(OutboundPayload::Notice(format!(
            "default model set to {}",
            self.current_model_label()
        )))
    }

    fn set_model_snapshot(&self, snapshot: ModelSnapshot) {
        *self.model.write().expect("model lock poisoned") = snapshot;
        self.prompt.clear_cache();
    }

    fn current_model_label(&self) -> String {
        let snapshot = self.model_snapshot();
        format!("{} ({})", snapshot.id, snapshot.profile.model_name)
    }

    fn default_model_id(&self) -> String {
        self.app_config
            .read()
            .expect("config lock poisoned")
            .llm
            .default
            .clone()
    }

    fn format_default_model(&self) -> String {
        let default = self.default_model_id();
        let current = self.model_snapshot().id;
        format!(
            "default model: {default}\ncurrent model: {current}\nType /switch default <provider/profile> to change the saved default."
        )
    }

    fn format_model_list(&self) -> String {
        let current = self.model_snapshot().id;
        let default = self.default_model_id();
        let mut out =
            format!("current model: {current}\ndefault model: {default}\nAvailable models:");
        for id in self.model_catalog.ids() {
            let current_marker = if id == current { "*" } else { " " };
            let default_marker = if id == default { " (default)" } else { "" };
            let _ = write!(out, "\n  {current_marker} {id}{default_marker}");
        }
        out.push_str(
            "\nType /switch <provider/profile> to switch now, or /switch default <provider/profile> to save the default.",
        );
        out
    }

    fn model_snapshot(&self) -> ModelSnapshot {
        self.model.read().expect("model lock poisoned").clone()
    }

    fn session_manager(&self, iter: &Iteration) -> &session::Manager {
        match iter.scope {
            SessionScope::Foreground => self.sessions.as_ref(),
            SessionScope::Cron => self.cron_sessions.as_ref(),
        }
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
        let model = self.model_snapshot();
        if !compact::should_compact(&messages, &model.profile, &self.compact_config) {
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
        let _ = self
            .hook
            .fire(
                HookEvent::PreCompact,
                None,
                serde_json::json!({ "trigger": "auto" }),
                &iter.session.0.to_string(),
                &self.cwd,
            )
            .await;
        match compact::compact_messages(
            messages.clone(),
            &model.profile,
            model.client.as_ref(),
            &self.compact_config,
            &mut state,
            CompactTrigger::Auto,
            &summary_system,
            self.timeout_secs,
        )
        .await
        {
            Ok((compacted, _report)) => {
                self.session_manager(iter)
                    .append_compaction(&iter.session, compacted.clone())
                    .await?;
                // Compaction rewrites the conversation prefix, so any
                // cached `iteration_system` sections are about to be
                // re-emitted into a brand-new context. Drop the cache
                // to mirror Claude Code's `clearSystemPromptSections`
                // behavior — same reasoning, same timing.
                self.prompt.clear_cache();
                let _ = self
                    .hook
                    .fire(
                        HookEvent::PostCompact,
                        None,
                        serde_json::json!({ "trigger": "auto" }),
                        &iter.session.0.to_string(),
                        &self.cwd,
                    )
                    .await;
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
        let records = self.sessions.load(&session).await?;
        let messages: Vec<Message> = records.into_iter().map(|r| r.message).collect();
        let summary_system = self.prompt.compact_summary_system(focus.as_deref());
        let _ = self
            .hook
            .fire(
                HookEvent::PreCompact,
                None,
                serde_json::json!({ "trigger": "manual" }),
                &session.0.to_string(),
                &self.cwd,
            )
            .await;
        let mut state = self.compact_state.lock().await;
        let model = self.model_snapshot();
        let result = compact::compact_messages(
            messages,
            &model.profile,
            model.client.as_ref(),
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
                self.sessions.append_compaction(&session, compacted).await?;
                // Same reasoning as the auto path in
                // `maybe_auto_compact`: the prefix changed, so the
                // section cache should rebuild.
                self.prompt.clear_cache();
                let _ = self
                    .hook
                    .fire(
                        HookEvent::PostCompact,
                        None,
                        serde_json::json!({ "trigger": "manual" }),
                        &session.0.to_string(),
                        &self.cwd,
                    )
                    .await;
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
        if !iter.is_visible() {
            return;
        }
        let msg = OutboundMessage::new(
            iter.channel.clone(),
            iter.session.clone(),
            OutboundPayload::Notice(text.to_string()),
        );
        let _ = self.out.send(msg).await;
    }

    async fn send_turn_end(&self, iter: &Iteration) -> Result<()> {
        if !iter.is_visible() {
            return Ok(());
        }
        let msg = OutboundMessage::new(
            iter.channel.clone(),
            iter.session.clone(),
            OutboundPayload::TurnEnd,
        );
        self.out.send(msg).await?;
        Ok(())
    }

    /// Execute one conversation iteration — from a user message to the
    /// persisted assistant reply, covering any number of LLM↔tool
    /// calls.
    async fn iteration(&self, iter: &Iteration, user_text: String) -> Result<String> {
        // UserPromptSubmit hook fires BEFORE we touch the session —
        // a blocking hook drops the message entirely, the user sees
        // an Error notice, no LLM call is made.
        let pre = self
            .hook
            .fire(
                HookEvent::UserPromptSubmit,
                None,
                serde_json::json!({ "prompt": user_text }),
                &iter.session.0.to_string(),
                &self.cwd,
            )
            .await;
        if pre.is_blocked() {
            let reason = pre
                .block_reason()
                .unwrap_or("blocked by UserPromptSubmit hook")
                .to_string();
            self.send_notice(iter, &format!("hook denied user message: {reason}"))
                .await;
            return Ok(String::new());
        }

        self.ensure_session(iter, &user_text).await?;
        self.session_manager(iter)
            .append(&iter.session, Message::User { content: user_text })
            .await?;

        let mut i: u8 = 0;
        loop {
            if let Some(cap) = self.config.max_iterations
                && i >= cap
            {
                return Err(Error::MaxIterationsExceeded(cap));
            }

            let messages = self.load_history(iter).await?;
            let messages = self.maybe_auto_compact(iter, messages).await?;
            // Prepend the freshly-built iteration system prompt
            // here rather than persisting it: the system prompt belongs
            // to request assembly, not transcript history. `MEMORY.md`
            // is injected separately as transient user context so edits
            // can take effect on the next request without invalidating
            // the cached system prefix.
            let messages = self.prepend_iteration_system(messages);
            let messages = self.inject_memory_context(messages).await?;
            let outcome = self.call(iter, messages).await?;
            let CallOutcome {
                content,
                thinking,
                tool_calls,
                ..
            } = outcome;

            let last_assistant_text = if content.is_empty() {
                String::new()
            } else {
                content.clone()
            };
            self.session_manager(iter)
                .append(
                    &iter.session,
                    Message::Assistant {
                        content: (!content.is_empty()).then_some(content),
                        tool_calls: tool_calls.clone(),
                        reasoning: thinking,
                    },
                )
                .await?;

            let no_more_calls = tool_calls.as_ref().is_none_or(Vec::is_empty);
            if no_more_calls {
                self.fire_stop_hook(iter, &last_assistant_text).await;
                return Ok(last_assistant_text);
            }
            // Safe to unwrap: `no_more_calls` short-circuited.
            let calls = tool_calls.unwrap_or_default();

            for call in calls {
                self.dispatch_one_with_hooks(iter, call).await?;
            }

            i = i.saturating_add(1);
        }
    }

    /// Invoke one tool call with `Pre/PostToolUse` hooks bracketing
    /// the actual dispatch. A blocked `PreToolUse` skips the call
    /// entirely and persists a synthetic tool-error message so the
    /// model sees its `tool_call_id` resolved on the next turn.
    async fn dispatch_one_with_hooks(&self, iter: &Iteration, call: ToolCall) -> Result<()> {
        let tool_input =
            serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap_or_default();
        if let Some(exec_id) = iter.exec_id.as_ref()
            && let Err(err) = self
                .exec
                .tool_call(
                    exec_id,
                    call.id.clone(),
                    call.name.clone(),
                    tool_input.clone(),
                )
                .await
        {
            eprintln!("[exec] failed to record tool call: {err}");
        }
        let pre_payload = serde_json::json!({
            "tool_name": call.name,
            "tool_input": tool_input,
            "tool_use_id": call.id,
        });
        let pre = self
            .hook
            .fire(
                HookEvent::PreToolUse,
                Some(&call.name),
                pre_payload,
                &iter.session.0.to_string(),
                &self.cwd,
            )
            .await;
        if pre.is_blocked() {
            let reason = pre.block_reason().unwrap_or("blocked by hook").to_string();
            let blocked_msg = Message::Tool {
                content: format!("{{\"error\":\"hook denied tool call: {reason}\"}}"),
                tool_call_id: call.id,
            };
            self.record_blocked_tool_result(iter, &call.name, &blocked_msg)
                .await;
            self.session_manager(iter)
                .append(&iter.session, blocked_msg)
                .await?;
            return Ok(());
        }

        let tool_name = call.name.clone();
        let tool_use_id = call.id.clone();
        let tool_input_raw = call.arguments.clone();
        let messages = if call.name == tools::task::TASK_RUN_TOOL_NAME {
            self.invoke_task_run_to_messages(iter, &call, &tool_input)
                .await
        } else {
            self.tools.invoke_to_messages(call).await
        };
        // The first message is the Tool reply; capture it before
        // moving the vec so we can include `tool_response` in the
        // PostToolUse payload.
        let tool_response_text = messages
            .first()
            .and_then(|m| match m {
                Message::Tool { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if let Some(exec_id) = iter.exec_id.as_ref()
            && let Err(err) = self
                .exec
                .tool_result(
                    exec_id,
                    tool_use_id.clone(),
                    tool_name.clone(),
                    tool_response_text.clone(),
                )
                .await
        {
            eprintln!("[exec] failed to record tool result: {err}");
        }
        for msg in messages {
            self.session_manager(iter)
                .append(&iter.session, msg)
                .await?;
        }

        let post_payload = serde_json::json!({
            "tool_name": tool_name,
            "tool_input": serde_json::from_str::<serde_json::Value>(&tool_input_raw)
                .unwrap_or(serde_json::Value::Null),
            "tool_response": tool_response_text,
            "tool_use_id": tool_use_id,
        });
        // PostToolUse outcome is informational v1 — a blocked post
        // hook can't undo a tool that already ran. Drop the result.
        let _ = self
            .hook
            .fire(
                HookEvent::PostToolUse,
                Some(&tool_name),
                post_payload,
                &iter.session.0.to_string(),
                &self.cwd,
            )
            .await;
        Ok(())
    }

    async fn record_blocked_tool_result(
        &self,
        iter: &Iteration,
        tool_name: &str,
        blocked_msg: &Message,
    ) {
        if let Some(exec_id) = iter.exec_id.as_ref()
            && let Message::Tool {
                content,
                tool_call_id,
            } = blocked_msg
            && let Err(err) = self
                .exec
                .tool_result(
                    exec_id,
                    tool_call_id.clone(),
                    tool_name.to_string(),
                    content.clone(),
                )
                .await
        {
            eprintln!("[exec] failed to record blocked tool result: {err}");
        }
    }

    async fn invoke_task_run_to_messages(
        &self,
        iter: &Iteration,
        call: &ToolCall,
        tool_input: &serde_json::Value,
    ) -> Vec<Message> {
        let content = match serde_json::from_value::<tools::task::TaskRunParams>(tool_input.clone())
        {
            Ok(params) => self.task_run_observation(iter, &params.task_id).await,
            Err(err) => serde_json::json!({
                "ok": false,
                "observation_type": "execution",
                "object": "task_run",
                "status": "failed",
                "error": format!("invalid task_run arguments: {err}"),
            }),
        };
        vec![Message::Tool {
            content: serialize_json(&content),
            tool_call_id: call.id.clone(),
        }]
    }

    async fn task_run_observation(&self, iter: &Iteration, task_id: &str) -> serde_json::Value {
        let task = match self.tasks.get(task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                return execution_observation(
                    None,
                    task_id,
                    exec::ExecStatus::Failed,
                    "",
                    Some("Task not found"),
                );
            }
            Err(err) => {
                let err_text = err.to_string();
                return execution_observation(
                    None,
                    task_id,
                    exec::ExecStatus::Failed,
                    "",
                    Some(&err_text),
                );
            }
        };

        let execution = exec::TaskExecution {
            task_id: task.id.clone(),
            task_subject: task.subject.clone(),
            prompt: prompt_for_direct_task(&task),
            trigger: exec::ExecTrigger::TaskRun,
        };
        let exec_id = match self
            .exec
            .start(execution.start(iter.session.clone(), iter.channel.clone()))
            .await
        {
            Ok(exec_id) => Some(exec_id),
            Err(err) => {
                eprintln!("[exec] failed to start task_run execution log: {err}");
                None
            }
        };

        match self
            .execute_task_silent(execution.prompt, exec_id.as_ref())
            .await
        {
            Ok(output) => {
                if let Some(exec_id) = exec_id.as_ref() {
                    if let Err(err) = self.exec.final_output(exec_id, output.clone()).await {
                        eprintln!("[exec] failed to write task_run final output: {err}");
                    }
                    if let Err(err) = self
                        .exec
                        .finish(exec_id, exec::ExecStatus::Succeeded, None)
                        .await
                    {
                        eprintln!("[exec] failed to finish task_run execution: {err}");
                    }
                }
                execution_observation(
                    exec_id.as_ref(),
                    &execution.task_id,
                    exec::ExecStatus::Succeeded,
                    &output,
                    None,
                )
            }
            Err(err) => {
                let err_text = err.to_string();
                if let Some(exec_id) = exec_id.as_ref()
                    && let Err(log_err) = self
                        .exec
                        .finish(exec_id, exec::ExecStatus::Failed, Some(err_text.clone()))
                        .await
                {
                    eprintln!("[exec] failed to mark task_run failed: {log_err}");
                }
                execution_observation(
                    exec_id.as_ref(),
                    &execution.task_id,
                    exec::ExecStatus::Failed,
                    "",
                    Some(&err_text),
                )
            }
        }
    }

    async fn execute_task_silent(
        &self,
        prompt: String,
        exec_id: Option<&exec::ExecId>,
    ) -> Result<String> {
        let mut messages = vec![Message::User { content: prompt }];
        let mut i: u8 = 0;
        loop {
            if let Some(cap) = self.config.max_iterations
                && i >= cap
            {
                return Err(Error::MaxIterationsExceeded(cap));
            }

            let request_messages = self
                .inject_memory_context(self.prepend_iteration_system(messages.clone()))
                .await?;
            let model = self.model_snapshot();
            let request = self.build_request(&model.profile, request_messages);
            let response = model.client.complete(request).await?;
            let content = response.content.unwrap_or_default();
            let tool_calls = response.tool_calls;
            let no_more_calls = tool_calls.as_ref().is_none_or(Vec::is_empty);
            messages.push(Message::Assistant {
                content: (!content.is_empty()).then_some(content.clone()),
                tool_calls: tool_calls.clone(),
                reasoning: response.thinking,
            });
            if no_more_calls {
                return Ok(content);
            }
            for call in tool_calls.unwrap_or_default() {
                let tool_messages = self.invoke_silent_tool(exec_id, call).await;
                messages.extend(tool_messages);
            }
            i = i.saturating_add(1);
        }
    }

    async fn invoke_silent_tool(
        &self,
        exec_id: Option<&exec::ExecId>,
        call: ToolCall,
    ) -> Vec<Message> {
        let tool_input =
            serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap_or_default();
        if let Some(exec_id) = exec_id
            && let Err(err) = self
                .exec
                .tool_call(
                    exec_id,
                    call.id.clone(),
                    call.name.clone(),
                    tool_input.clone(),
                )
                .await
        {
            eprintln!("[exec] failed to record task_run tool call: {err}");
        }
        let messages = if call.name == tools::task::TASK_RUN_TOOL_NAME {
            vec![Message::Tool {
                content: serialize_json(&serde_json::json!({
                    "ok": false,
                    "observation_type": "execution",
                    "object": "task_run",
                    "status": "failed",
                    "error": "nested task_run is not supported",
                })),
                tool_call_id: call.id.clone(),
            }]
        } else {
            self.tools.invoke_to_messages(call.clone()).await
        };
        if let Some(exec_id) = exec_id {
            for message in &messages {
                if let Message::Tool {
                    content,
                    tool_call_id,
                } = message
                    && let Err(err) = self
                        .exec
                        .tool_result(
                            exec_id,
                            tool_call_id.clone(),
                            call.name.clone(),
                            content.clone(),
                        )
                        .await
                {
                    eprintln!("[exec] failed to record task_run tool result: {err}");
                }
            }
        }
        messages
    }

    /// Fire the `Stop` hook with the last assistant text. Outcome
    /// ignored — `Stop` is post-iteration informational only.
    async fn fire_stop_hook(&self, iter: &Iteration, last_assistant_message: &str) {
        let _ = self
            .hook
            .fire(
                HookEvent::Stop,
                None,
                serde_json::json!({
                    "stop_hook_active": false,
                    "last_assistant_message": last_assistant_message,
                }),
                &iter.session.0.to_string(),
                &self.cwd,
            )
            .await;
    }

    /// Issue one LLM call within an iteration and aggregate the
    /// resulting stream.
    async fn call(&self, iter: &Iteration, messages: Vec<Message>) -> Result<CallOutcome> {
        let model = self.model_snapshot();
        let request = self.build_request(&model.profile, messages);
        let stream = model.client.stream(request).await?;
        self.stream_to_channel(iter, stream).await
    }

    /// Render the iteration system prompt and prepend it to
    /// `messages`. Called from [`Self::iteration`] only — the title
    /// prompt and must not see the iteration system block.
    fn prepend_iteration_system(&self, mut messages: Vec<Message>) -> Vec<Message> {
        let model = self.model_snapshot();
        let ctx = PromptContext {
            model_id: &model.profile.model_name,
            cwd: &self.cwd,
        };
        let system = self.prompt.iteration_system(&ctx).into_message();
        let mut out = Vec::with_capacity(messages.len() + 1);
        out.push(system);
        out.append(&mut messages);
        out
    }

    async fn inject_memory_context(&self, messages: Vec<Message>) -> Result<Vec<Message>> {
        let Some(content) = self.memory.render_user_context(&self.config.memory).await? else {
            return Ok(messages);
        };
        Ok(insert_user_context_after_system(messages, content))
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
                if iter.is_visible() {
                    let msg = OutboundMessage::new(
                        iter.channel.clone(),
                        iter.session.clone(),
                        OutboundPayload::ThinkingDelta { stream_id, delta },
                    );
                    self.out.send(msg).await?;
                }
            }

            if let Some(delta) = chunk.content_delta {
                content.push_str(&delta);
                if iter.is_visible() {
                    let msg = OutboundMessage::new(
                        iter.channel.clone(),
                        iter.session.clone(),
                        OutboundPayload::ReplyDelta { stream_id, delta },
                    );
                    self.out.send(msg).await?;
                }
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

        if iter.is_visible() {
            let end = OutboundMessage::new(
                iter.channel.clone(),
                iter.session.clone(),
                OutboundPayload::ReplyEnd { stream_id },
            );
            self.out.send(end).await?;
        }

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
        let sessions = self.session_manager(iter);
        if sessions.metadata(&iter.session).await?.is_some() {
            return Ok(());
        }
        let title = match self.generate_title(first_text).await {
            Ok(t) if !t.is_empty() => t,
            _ => fallback_title(first_text),
        };
        sessions
            .create_with_identity(
                &iter.session,
                title,
                iter.channel.clone(),
                iter.peer_id.clone(),
                iter.account_id.clone(),
                iter.guild_id.clone(),
            )
            .await?;
        // SessionStart hook fires once per fresh session — never on
        // resume, never per iteration. `source: startup` distinguishes
        // a brand-new session from a `compact`-triggered one (we don't
        // emit `compact` here; future Pre/PostCompact wiring may add
        // an explicit replay if useful).
        let _ = self
            .hook
            .fire(
                HookEvent::SessionStart,
                None,
                serde_json::json!({ "source": "startup" }),
                &iter.session.0.to_string(),
                &self.cwd,
            )
            .await;
        Ok(())
    }

    /// Generate a short session title from the first user message
    /// using a non-streaming completion.
    async fn generate_title(&self, user_input: &str) -> Result<String> {
        let model = self.model_snapshot();
        let mut request =
            self.build_request(&model.profile, self.prompt.title_messages(user_input));
        // Title generation overrides the profile defaults: no tools
        // advertised, tighter token budget. Temperature is left to
        // the provider — not every API honors it (DeepSeek's
        // thinking mode silently drops it, for instance), and a
        // bare title is short enough that sampling jitter doesn't
        // matter.
        request.tools = Vec::new();
        request.max_tokens = Some(TITLE_MAX_TOKENS);
        let response = model.client.complete(request).await?;
        Ok(response
            .content
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .to_string())
    }

    /// Project the session's record stream into the flat
    /// [`Message`] sequence the LLM request consumes.
    async fn load_history(&self, iter: &Iteration) -> Result<Vec<Message>> {
        let records = self.session_manager(iter).load(&iter.session).await?;
        Ok(records.into_iter().map(|r| r.message).collect())
    }

    /// Fill a fresh [`Request`] from the active profile plus the given
    /// message history and the registry's current tool schemas.
    ///
    /// `timeout_secs` is a transport-level concern and therefore pulled
    /// from the shared [`crate::config::LLMConfig::timeout_secs`] rather
    /// than the per-profile block.
    fn build_request(&self, profile: &LLMProfile, messages: Vec<Message>) -> Request {
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
        let thinking = profile.thinking.map(|enabled| Thinking {
            enabled,
            reasoning_effort: None,
        });
        Request {
            messages,
            tools: self.tools.schemas(),
            model_name: profile.model_name.clone(),
            max_tokens: profile.max_tokens,
            temperature: profile.temperature,
            timeout_secs: self.timeout_secs,
            thinking,
        }
    }
}

fn insert_user_context_after_system(mut messages: Vec<Message>, content: String) -> Vec<Message> {
    if content.trim().is_empty() {
        return messages;
    }
    let insert_at = usize::from(matches!(messages.first(), Some(Message::System { .. })));
    messages.insert(insert_at, Message::User { content });
    messages
}

fn prompt_for_direct_task(task: &task::Task) -> String {
    format!(
        "# Task: {}\n\nTask ID: {}\n\n{}",
        task.subject,
        task.id,
        task.description.trim()
    )
}

fn execution_observation(
    exec_id: Option<&exec::ExecId>,
    task_id: &str,
    status: exec::ExecStatus,
    output: &str,
    error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "ok": status == exec::ExecStatus::Succeeded,
        "observation_type": "execution",
        "object": "task_run",
        "exec_id": exec_id.map(ToString::to_string),
        "task_id": task_id,
        "status": exec_status_name(status),
        "output": output,
        "error": error,
    })
}

fn exec_status_name(status: exec::ExecStatus) -> &'static str {
    match status {
        exec::ExecStatus::Succeeded => "succeeded",
        exec::ExecStatus::Failed => "failed",
        exec::ExecStatus::Skipped => "skipped",
    }
}

fn serialize_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value always serializes")
}

fn with_timer_context(input: String, timer_id: Option<&str>) -> String {
    let Some(timer_id) = timer_id else {
        return input;
    };
    format!("# Background timer run\n\nTimer ID: {timer_id}\n\n{input}")
}

fn should_notify_background(output: &str) -> bool {
    let trimmed = output.trim();
    !trimmed.is_empty() && !trimmed.to_ascii_uppercase().contains(SILENT_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_context_is_inserted_after_system_message() {
        let messages = vec![
            Message::System {
                content: "system".to_string(),
            },
            Message::User {
                content: "old".to_string(),
            },
            Message::Assistant {
                content: Some("older answer".to_string()),
                tool_calls: None,
                reasoning: None,
            },
            Message::User {
                content: "current".to_string(),
            },
        ];
        let messages =
            insert_user_context_after_system(messages, "# User Memory\n\n- x".to_string());

        let Message::System { content } = &messages[0] else {
            panic!("expected system message");
        };
        assert_eq!(content, "system");
        let Message::User { content } = &messages[1] else {
            panic!("expected memory user-context message");
        };
        assert_eq!(content, "# User Memory\n\n- x");
        let Message::User { content } = &messages[4] else {
            panic!("expected user message");
        };
        assert_eq!(content, "current");
    }

    #[test]
    fn empty_memory_context_leaves_messages_unchanged() {
        let messages = vec![Message::System {
            content: "system".to_string(),
        }];
        let updated = insert_user_context_after_system(messages.clone(), " \n".to_string());
        let Message::System { content } = &updated[0] else {
            panic!("expected system message");
        };
        assert_eq!(content, "system");
    }

    #[test]
    fn background_notification_respects_silent_marker() {
        assert!(!should_notify_background(""));
        assert!(!should_notify_background("  [SILENT]  "));
        assert!(!should_notify_background("done\n\n[silent]"));
        assert!(should_notify_background("Review the blocked timer."));
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
