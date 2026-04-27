//! CLI channel — logic / data layer for the terminal UI.
//!
//! Translates [`crossterm`] input events into [`CliState`] mutations
//! and folds [`crate::bus::OutboundPayload`]s into transcript updates.
//! Ratatui rendering lives in the [`tui`] submodule; everything in
//! this file is framework-agnostic state + event wiring.
//!
//! Input buffer is a [`ratatui_textarea::TextArea`], which gives us
//! proper cursor movement, multi-byte-safe editing, word-level
//! delete, undo / redo, and so on for free. We intercept
//! `Enter` / `Esc` / `Ctrl-C` / `Ctrl-D` before the textarea sees
//! them (those are REPL-level controls, not edit operations);
//! everything else is forwarded via [`TextArea::input`].
//!
//! Interior mutability: [`CliState`] sits behind `Arc<Mutex<_>>` so
//! the input loop (inside [`CliChannel::start`]) and the render task
//! (spawned inside [`CliChannel::start`]) can both mutate it. The
//! render task is woken by [`tokio::sync::Notify`] — any mutation
//! ends with `self.redraw.notify_one()`.

pub mod tui;

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui_textarea::TextArea;
use tokio::sync::Notify;

use crate::bus::{
    ChannelID, InboundMessage, InboundPayload, InboundSender, OutboundMessage, OutboundPayload,
};
use crate::channels::{Channel, Error, Result};
use crate::command::slash::{self, SlashCommand};
use crate::llm::Message;
use crate::session;

/// Number of logical lines moved per keyboard arrow press.
const KEY_SCROLL_STEP: u16 = 1;

/// Lines scrolled per mouse-wheel tick. Coarser than arrow-key
/// scrolling so a wheel notch moves a small chunk instead of a single
/// line.
const WHEEL_STEP: u16 = 3;

fn is_ctrl_char(key: &crossterm::event::KeyEvent, target: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&target))
}

/// One finalized transcript entry.
#[derive(Debug, Clone)]
pub enum Line {
    /// User input, echoed to the transcript on submit.
    User(String),
    /// Assistant reply — either a one-shot [`OutboundPayload::Reply`]
    /// or the finalized stream collected between
    /// [`OutboundPayload::ReplyDelta`] and
    /// [`OutboundPayload::ReplyEnd`].
    Assistant(String),
    /// Reasoning trace from a thinking-capable model, finalized after
    /// the same `ReplyEnd` that closes its paired `Assistant` entry.
    /// Rendered dimmed inline above the assistant reply.
    //
    // TODO(reasoning-folding): expose `/show-reasoning` or similar so
    // the user can toggle visibility per-turn or globally — the TUI
    // currently always renders the full trace inline.
    Thinking(String),
    /// Compact-pipeline summary boundary — replaces a swath of older
    /// history. Rendered with a distinct prefix so the user can see
    /// where the conversation was condensed.
    Compact(String),
    /// Error surfaced via the bus, a tool failure, or an unknown
    /// slash command.
    Error(String),
}

/// Where the per-turn state machine currently sits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Awaiting user input.
    #[default]
    Idle,
    /// Agent is processing a turn. Regular messages are queued;
    /// slash commands (`/help`, `/exit`) still work.
    Replying,
}

/// Which modal overlay, if any, is covering the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// Help overlay listing commands and keybindings.
    Help,
    /// Skills overlay listing every loaded SKILL.md by name +
    /// description. Triggered by `/skills`.
    Skills,
}

/// UI state shared between the input loop and the render task.
pub struct CliState {
    /// Finalized transcript entries, chronological.
    pub transcript: Vec<Line>,
    /// Streaming assistant content in progress.
    pub streaming: Option<String>,
    /// Streaming chain-of-thought in progress, fed by
    /// [`OutboundPayload::ThinkingDelta`]. Rendered above the in-flight
    /// `streaming` reply, dimmed.
    pub streaming_thinking: Option<String>,
    /// What the user is currently typing. Backed by
    /// [`TextArea`] for proper cursor + editing behavior.
    pub input: TextArea<'static>,
    /// Regular user inputs submitted while the agent is still busy.
    /// The CLI owns the real queue; the TUI layer only renders a
    /// derived preview, matching Codex's chat-widget/bottom-pane split.
    pub queued_inputs: VecDeque<String>,
    /// Idle or Replying.
    pub mode: Mode,
    /// Active modal overlay, if any.
    pub overlay: Option<Overlay>,
    /// Whether reasoning traces from thinking-capable models are
    /// rendered in the TUI. The data is still kept in transcript
    /// state and persisted session history.
    pub show_thinking: bool,
    /// Scroll offset for the active overlay. Kept separate from
    /// transcript scroll so opening help/skills does not disturb the
    /// conversation viewport underneath.
    pub overlay_scroll_offset: u16,
    /// Conversation scroll offset in logical lines from the top.
    /// Authoritative only when [`Self::follow_bottom`] is `false`; in
    /// follow mode it is kept in sync with the render-computed
    /// `max_offset` so a subsequent arrow-up moves up from the current
    /// bottom view rather than from zero.
    pub scroll_offset: u16,
    /// When `true`, the renderer ignores [`Self::scroll_offset`] and
    /// always shows the bottom of the transcript (auto-follow). Set
    /// back to `true` when the user pages down to the bottom.
    pub follow_bottom: bool,
    /// Snapshot of `(name, description)` pairs from the boot-time
    /// [`crate::skill::SkillIndex`]. Empty when no skills are
    /// loaded; used by [`Overlay::Skills`] rendering only — the
    /// `/<name>` fallback hits the live `Arc<SkillIndex>` on
    /// [`CliChannel`] directly.
    pub skills: Vec<(String, String)>,
}

impl Default for CliState {
    fn default() -> Self {
        let mut input = TextArea::default();
        // Default cursor-line style underlines the whole active row;
        // kill it — our textarea is always single-line, the cursor
        // itself is visible enough.
        input.set_style(Style::default());
        input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        input.set_cursor_line_style(Style::default());
        input.set_selection_style(Style::default().bg(Color::DarkGray));
        Self {
            transcript: Vec::new(),
            streaming: None,
            streaming_thinking: None,
            input,
            queued_inputs: VecDeque::new(),
            mode: Mode::default(),
            overlay: None,
            show_thinking: true,
            overlay_scroll_offset: 0,
            scroll_offset: 0,
            follow_bottom: true,
            skills: Vec::new(),
        }
    }
}

impl CliState {
    fn queue_input(&mut self, text: String) {
        self.queued_inputs.push_back(text);
    }

    fn pop_next_queued_input(&mut self) -> Option<String> {
        self.queued_inputs.pop_front()
    }

    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.overlay_scroll_offset = 0;
    }

    fn scroll_overlay_up(&mut self, amount: u16) {
        self.overlay_scroll_offset = self.overlay_scroll_offset.saturating_sub(amount);
    }

    fn scroll_overlay_down(&mut self, amount: u16) {
        self.overlay_scroll_offset = self.overlay_scroll_offset.saturating_add(amount);
    }

    fn scroll_transcript_up(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        self.follow_bottom = false;
    }

    fn scroll_transcript_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
    }

    fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    fn input_is_empty(&self) -> bool {
        self.input_text().trim().is_empty()
    }

    fn clear_input(&mut self) -> bool {
        if self.input_text().is_empty() {
            return false;
        }
        self.input.clear();
        self.follow_bottom = true;
        true
    }

    fn insert_input_newline(&mut self) {
        self.input.insert_newline();
    }

    fn cursor_preceded_by_backslash(&self) -> bool {
        let cursor = self.input.cursor();
        let row = cursor.0;
        let col = cursor.1;
        let Some(line) = self.input.lines().get(row) else {
            return false;
        };
        col > 0 && line.chars().nth(col - 1) == Some('\\')
    }

    fn replace_backslash_with_newline(&mut self) -> bool {
        if !self.cursor_preceded_by_backslash() {
            return false;
        }
        self.input.delete_char();
        self.input.insert_newline();
        true
    }
}

/// Constant peer id stamped on every inbound message from the local
/// CLI — there is exactly one user per terminal, so the peer
/// dimension is fixed. Future IM channels fill this with the
/// platform-provided user id.
const CLI_PEER_ID: &str = "cli-user";

/// Terminal UI channel.
///
/// The channel no longer owns a `SessionID`; the gateway is the
/// session authority. The channel tags inbound messages with its
/// [`ChannelID`] and [`CLI_PEER_ID`] identity, and the gateway
/// looks up (or creates) the bound session before the message
/// reaches the agent.
///
/// The channel holds a read-capable handle to the session store
/// ([`sessions`](Self::sessions)) so it can rebuild its transcript
/// when the gateway announces a session switch via
/// [`OutboundPayload::SessionSwitched`].
pub struct CliChannel {
    id: ChannelID,
    state: Arc<Mutex<CliState>>,
    redraw: Arc<Notify>,
    inbound: Arc<Mutex<Option<InboundSender>>>,
    /// Session store handle. The channel only reads from this (to
    /// replay history on [`OutboundPayload::SessionSwitched`]); the
    /// gateway and agent are the write authorities.
    sessions: Arc<session::Manager>,
    /// Skill catalog used for two purposes: rendering the `/skills`
    /// overlay, and the `/<skill-name>` slash-command fallback that
    /// expands a skill body into a regular user message.
    skills: Arc<crate::skill::SkillIndex>,
}

impl CliChannel {
    /// Construct a channel tagged with the given id.
    ///
    /// `sessions` is used to replay history when the gateway
    /// announces a session switch; the CLI does not write to it.
    /// `skills` powers `/skills` (overlay) and the `/<name>`
    /// slash-command fallback.
    #[must_use]
    pub fn new(
        id: ChannelID,
        sessions: Arc<session::Manager>,
        skills: Arc<crate::skill::SkillIndex>,
        show_thinking: bool,
    ) -> Self {
        let skill_snapshot: Vec<(String, String)> = skills
            .entries()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect();
        let state = CliState {
            skills: skill_snapshot,
            show_thinking,
            ..CliState::default()
        };
        Self {
            id,
            state: Arc::new(Mutex::new(state)),
            redraw: Arc::new(Notify::new()),
            inbound: Arc::new(Mutex::new(None)),
            sessions,
            skills,
        }
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn id(&self) -> &ChannelID {
        &self.id
    }

    async fn start(&self, inbound: InboundSender) -> Result<()> {
        *self.inbound.lock().unwrap() = Some(inbound.clone());
        let _guard = TerminalGuard::enter()?;

        // Render task — sole owner of the Terminal handle.
        let render_state = self.state.clone();
        let render_notify = self.redraw.clone();
        let render_task = tokio::spawn(async move {
            let backend = CrosstermBackend::new(io::stdout());
            let mut terminal = Terminal::new(backend)?;
            loop {
                render_notify.notified().await;
                let mut st = render_state.lock().unwrap();
                if terminal.draw(|f| tui::render(f, &mut st)).is_err() {
                    break;
                }
            }
            Ok::<_, io::Error>(())
        });

        // Paint the first frame before any input arrives.
        self.redraw.notify_one();

        // Input loop — drive until the user exits or the bus closes.
        let mut events = EventStream::new();
        while let Some(ev_res) = events.next().await {
            let ev = ev_res.map_err(Error::from)?;
            if !self.handle_event(ev, &inbound).await? {
                break;
            }
        }

        render_task.abort();
        let _ = render_task.await;
        *self.inbound.lock().unwrap() = None;
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        apply_outbound(&self.state, &self.sessions, msg.payload).await?;
        self.maybe_send_next_queued_input().await?;
        self.redraw.notify_one();
        Ok(())
    }
}

impl CliChannel {
    /// Process one crossterm event. `Ok(false)` exits the input loop.
    async fn handle_event(&self, event: Event, inbound: &InboundSender) -> Result<bool> {
        match event {
            Event::Mouse(mouse) => Ok(self.handle_mouse(mouse)),
            Event::Key(key) => self.handle_key(key, inbound).await,
            _ => Ok(true),
        }
    }

    async fn handle_key(&self, key: KeyEvent, inbound: &InboundSender) -> Result<bool> {
        // Windows terminals emit Press + Release; we only act on Press.
        if key.kind != KeyEventKind::Press {
            return Ok(true);
        }

        if let Some(keep_running) = self.handle_global_key(&key) {
            return Ok(keep_running);
        }

        if self.handle_overlay_key(&key) {
            return Ok(true);
        }

        // Conversation scroll is intercepted before textarea input so
        // Up / Down move the transcript view. Enter submits; every
        // other key is an edit operation forwarded to the textarea.
        match key.code {
            KeyCode::Up => {
                let mut state = self.state.lock().unwrap();
                state.scroll_transcript_up(KEY_SCROLL_STEP);
                drop(state);
                self.redraw.notify_one();
                Ok(true)
            }
            KeyCode::Down => {
                // Renderer clamps this to `max_offset` and flips
                // `follow_bottom = true` once the clamped value equals
                // the max, so over-scrolling naturally re-enters
                // follow mode without extra bookkeeping here.
                let mut state = self.state.lock().unwrap();
                state.scroll_transcript_down(KEY_SCROLL_STEP);
                drop(state);
                self.redraw.notify_one();
                Ok(true)
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                let mut state = self.state.lock().unwrap();
                state.insert_input_newline();
                drop(state);
                self.redraw.notify_one();
                Ok(true)
            }
            KeyCode::Enter => {
                let continued = {
                    let mut state = self.state.lock().unwrap();
                    state.replace_backslash_with_newline()
                };
                if continued {
                    self.redraw.notify_one();
                    Ok(true)
                } else {
                    self.handle_submit(inbound).await
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let changed = self.state.lock().unwrap().input.delete_line_by_head();
                if changed {
                    self.redraw.notify_one();
                }
                Ok(true)
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let changed = self.state.lock().unwrap().input.undo();
                if changed {
                    self.redraw.notify_one();
                }
                Ok(true)
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let changed = self.state.lock().unwrap().input.redo();
                if changed {
                    self.redraw.notify_one();
                }
                Ok(true)
            }
            _ => {
                let changed = self.state.lock().unwrap().input.input(key);
                if changed {
                    self.redraw.notify_one();
                }
                Ok(true)
            }
        }
    }

    fn handle_global_key(&self, key: &KeyEvent) -> Option<bool> {
        if is_ctrl_char(key, 'c') {
            return Some(self.handle_ctrl_c());
        }

        if is_ctrl_char(key, 'd') {
            return Some(self.handle_ctrl_d());
        }

        if is_ctrl_char(key, 'l') {
            self.redraw.notify_one();
            return Some(true);
        }

        if key.code == KeyCode::Esc {
            self.handle_escape();
            return Some(true);
        }

        None
    }

    fn handle_overlay_key(&self, key: &KeyEvent) -> bool {
        if self.state.lock().unwrap().overlay.is_none() {
            return false;
        }

        match key.code {
            KeyCode::Up => {
                self.state
                    .lock()
                    .unwrap()
                    .scroll_overlay_up(KEY_SCROLL_STEP);
                self.redraw.notify_one();
                true
            }
            KeyCode::Down => {
                self.state
                    .lock()
                    .unwrap()
                    .scroll_overlay_down(KEY_SCROLL_STEP);
                self.redraw.notify_one();
                true
            }
            _ => false,
        }
    }

    /// Handle one mouse event. Currently only wheel scroll is wired;
    /// clicks / drags are ignored so the terminal's own selection
    /// bypass (`Shift`+drag on most emulators) stays usable.
    fn handle_mouse(&self, mouse: MouseEvent) -> bool {
        if self.state.lock().unwrap().overlay.is_some() {
            let mut state = self.state.lock().unwrap();
            match mouse.kind {
                MouseEventKind::ScrollUp => state.scroll_overlay_up(WHEEL_STEP),
                MouseEventKind::ScrollDown => state.scroll_overlay_down(WHEEL_STEP),
                _ => return true,
            }
            drop(state);
            self.redraw.notify_one();
            return true;
        }
        let mut state = self.state.lock().unwrap();
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                state.scroll_transcript_up(WHEEL_STEP);
            }
            MouseEventKind::ScrollDown => {
                state.scroll_transcript_down(WHEEL_STEP);
            }
            _ => return true,
        }
        drop(state);
        self.redraw.notify_one();
        true
    }

    fn handle_escape(&self) {
        let mut state = self.state.lock().unwrap();
        if state.overlay.is_some() {
            state.overlay = None;
            state.overlay_scroll_offset = 0;
        } else if !state.clear_input() && state.mode == Mode::Replying {
            // TODO(interrupt): once the agent grows a cancellation
            // path, publish InboundPayload::Interrupt here so the
            // current iteration aborts. For MS0 this keystroke is
            // silently swallowed.
        }
        drop(state);
        self.redraw.notify_one();
    }

    fn handle_ctrl_c(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let changed = if state.overlay.is_some() {
            state.overlay = None;
            state.overlay_scroll_offset = 0;
            true
        } else {
            state.clear_input()
        };
        drop(state);
        if changed {
            self.redraw.notify_one();
        }
        true
    }

    fn handle_ctrl_d(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.overlay.is_some() {
            state.overlay = None;
            state.overlay_scroll_offset = 0;
            drop(state);
            self.redraw.notify_one();
            return true;
        }
        if state.input_is_empty() {
            return false;
        }

        let changed = state.input.delete_next_char();
        drop(state);
        if changed {
            self.redraw.notify_one();
        }
        true
    }

    /// Handle Enter. Commands (`/xxx`) always run. Regular messages
    /// submit immediately when idle; while replying, they are queued
    /// as follow-up input and shown in the composer preview.
    async fn handle_submit(&self, inbound: &InboundSender) -> Result<bool> {
        let (payload, command) = {
            let mut state = self.state.lock().unwrap();
            let text = state.input_text();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(true);
            }

            let payload = trimmed.to_string();
            let command = payload.strip_prefix('/').map(|s| s.trim().to_string());

            if command.is_none() && state.mode == Mode::Replying {
                state.queue_input(payload);
                state.input.clear();
                state.follow_bottom = true;
                drop(state);
                self.redraw.notify_one();
                return Ok(true);
            }

            // Clear input after consumption (preserves TextArea config —
            // cursor style, block if set, etc.) AND snap the conversation
            // back to follow-bottom: user just submitted, so whatever
            // lands next (echoed user line, command overlay / error, or
            // a streaming reply) must be visible. Outbound payloads from
            // the agent do NOT trigger this — if the user scrolled up to
            // read old content during a stream, their view stays frozen.
            state.input.clear();
            state.follow_bottom = true;
            (payload, command)
        };
        self.redraw.notify_one();

        if let Some(cmd) = command {
            return Ok(self.dispatch_command(&cmd, inbound).await);
        }

        {
            let mut state = self.state.lock().unwrap();
            state.transcript.push(Line::User(payload.clone()));
            state.mode = Mode::Replying;
        }
        self.redraw.notify_one();

        let msg = InboundMessage::with_peer(
            self.id.clone(),
            CLI_PEER_ID,
            InboundPayload::UserInput(payload),
        );
        if inbound.send(msg).await.is_err() {
            return Ok(false);
        }
        Ok(true)
    }

    async fn maybe_send_next_queued_input(&self) -> Result<()> {
        let Some(inbound) = self.inbound.lock().unwrap().clone() else {
            return Ok(());
        };

        let payload = {
            let mut state = self.state.lock().unwrap();
            if state.mode != Mode::Idle {
                return Ok(());
            }
            let Some(payload) = state.pop_next_queued_input() else {
                return Ok(());
            };
            state.transcript.push(Line::User(payload.clone()));
            state.mode = Mode::Replying;
            state.follow_bottom = true;
            payload
        };
        self.redraw.notify_one();

        let msg = InboundMessage::with_peer(
            self.id.clone(),
            CLI_PEER_ID,
            InboundPayload::UserInput(payload),
        );
        inbound.send(msg).await?;
        Ok(())
    }

    /// Execute one slash command. CLI-local commands mutate UI
    /// state directly; everything else has already been validated by
    /// the clap-backed parser and then falls through to skill lookup
    /// or the gateway/agent command chain.
    async fn dispatch_command(&self, body: &str, inbound: &InboundSender) -> bool {
        let parsed = match slash::parse(body) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.state.lock().unwrap().transcript.push(Line::Error(err));
                self.redraw.notify_one();
                return true;
            }
        };

        match parsed {
            SlashCommand::Help => {
                self.state.lock().unwrap().open_overlay(Overlay::Help);
                self.redraw.notify_one();
                true
            }
            SlashCommand::Skills => {
                self.state.lock().unwrap().open_overlay(Overlay::Skills);
                self.redraw.notify_one();
                true
            }
            SlashCommand::Exit | SlashCommand::Quit => false,
            SlashCommand::External { name, .. } => {
                self.maybe_dispatch_skill_or_forward(&name, body, inbound)
                    .await
            }
            SlashCommand::New
            | SlashCommand::List
            | SlashCommand::Load { .. }
            | SlashCommand::Switch(_)
            | SlashCommand::Compact { .. }
            | SlashCommand::Heartbeat(_)
            | SlashCommand::Cron(_) => self.forward_command(body, inbound).await,
        }
    }

    /// Skill fallback for unknown slash commands. The lookup key is
    /// supplied by the shared clap parser. A hit expands the
    /// SKILL.md body into a regular [`InboundPayload::UserInput`];
    /// a miss falls through to [`InboundPayload::Command`] so the
    /// agent layer can report an unknown command.
    async fn maybe_dispatch_skill_or_forward(
        &self,
        name: &str,
        body: &str,
        inbound: &InboundSender,
    ) -> bool {
        if let Some(skill) = self.skills.get(name) {
            // Echo the invocation so the user sees they triggered it,
            // then ship the SKILL.md body as if it were typed.
            {
                let mut state = self.state.lock().unwrap();
                state
                    .transcript
                    .push(Line::User(format!("/{}", skill.frontmatter.name)));
                state.mode = Mode::Replying;
            }
            self.redraw.notify_one();

            let payload = InboundMessage::with_peer(
                self.id.clone(),
                CLI_PEER_ID,
                InboundPayload::UserInput(skill.body.clone()),
            );
            return inbound.send(payload).await.is_ok();
        }

        self.forward_command(body, inbound).await
    }

    async fn forward_command(&self, body: &str, inbound: &InboundSender) -> bool {
        let forwarded = InboundMessage::with_peer(
            self.id.clone(),
            CLI_PEER_ID,
            InboundPayload::Command(body.to_string()),
        );
        inbound.send(forwarded).await.is_ok()
    }
}

/// Fold one outbound payload into the UI state.
///
/// Async because [`OutboundPayload::SessionSwitched`] requires
/// reading session history off disk before updating the transcript.
/// All other variants are synchronous over the held lock.
async fn apply_outbound(
    state: &Arc<Mutex<CliState>>,
    sessions: &Arc<session::Manager>,
    payload: OutboundPayload,
) -> Result<()> {
    // SessionSwitched is the only async arm; handle it separately so
    // the sync arms can hold the `std::sync::Mutex` guard without
    // ever crossing an `.await` boundary (tokio warns about that,
    // and it would deadlock if the lock were ever contended from
    // inside the render task).
    if let OutboundPayload::SessionSwitched(id) = payload {
        let records = sessions.load(&id).await?;
        let mut st = state.lock().unwrap();
        st.transcript.clear();
        st.streaming = None;
        st.streaming_thinking = None;
        st.queued_inputs.clear();
        st.mode = Mode::Idle;
        for record in records {
            push_record_as_line(&mut st.transcript, record.message);
        }
        return Ok(());
    }

    let mut state = state.lock().unwrap();
    match payload {
        OutboundPayload::ThinkingDelta { delta, .. } => {
            state
                .streaming_thinking
                .get_or_insert_with(String::new)
                .push_str(&delta);
        }
        OutboundPayload::ReplyDelta { delta, .. } => {
            state
                .streaming
                .get_or_insert_with(String::new)
                .push_str(&delta);
        }
        OutboundPayload::ReplyEnd { .. } => {
            if let Some(thinking) = state.streaming_thinking.take() {
                state.transcript.push(Line::Thinking(thinking));
            }
            if let Some(content) = state.streaming.take() {
                state.transcript.push(Line::Assistant(content));
            }
        }
        OutboundPayload::TurnEnd => {
            if let Some(thinking) = state.streaming_thinking.take() {
                state.transcript.push(Line::Thinking(thinking));
            }
            if let Some(content) = state.streaming.take() {
                state.transcript.push(Line::Assistant(content));
            }
            state.mode = Mode::Idle;
        }
        OutboundPayload::Reply(text) => {
            if let Some(thinking) = state.streaming_thinking.take() {
                state.transcript.push(Line::Thinking(thinking));
            }
            if let Some(content) = state.streaming.take() {
                state.transcript.push(Line::Assistant(content));
            }
            state.transcript.push(Line::Assistant(text));
            state.mode = Mode::Idle;
        }
        OutboundPayload::Error(err) => {
            if let Some(thinking) = state.streaming_thinking.take() {
                state.transcript.push(Line::Thinking(thinking));
            }
            if let Some(content) = state.streaming.take() {
                state.transcript.push(Line::Assistant(content));
            }
            state.transcript.push(Line::Error(err));
        }
        OutboundPayload::Notice(text) => {
            // Ambient system message (e.g. gateway command feedback).
            // Doesn't end an in-flight reply and doesn't transition
            // mode — notices can arrive any time without implying a
            // stream boundary.
            state.transcript.push(Line::Assistant(text));
        }
        OutboundPayload::SessionSwitched(_) => {
            // Matched above. Unreachable by construction.
        }
    }
    Ok(())
}

/// Project one persisted [`Message`] into the transcript. System
/// prompts and raw tool exchanges are omitted because they are
/// internal plumbing — the transcript shows the user-visible
/// conversation only. An assistant message that carries no text
/// (only tool calls) is also skipped to avoid empty bubbles.
fn push_record_as_line(transcript: &mut Vec<Line>, msg: Message) {
    match msg {
        Message::User { content } => transcript.push(Line::User(content)),
        Message::Assistant {
            content, reasoning, ..
        } => {
            if let Some(thinking) = reasoning {
                transcript.push(Line::Thinking(thinking));
            }
            if let Some(text) = content {
                transcript.push(Line::Assistant(text));
            }
        }
        Message::Compact(boundary) => {
            // Render the summary inline so the user sees what was
            // injected. Future work could add a folding control;
            // see the `Line::Thinking` precedent.
            transcript.push(Line::Compact(boundary.summary));
        }
        Message::System { .. } | Message::Tool { .. } => {}
    }
}

/// RAII guard for terminal setup. Enters raw mode + the alternate
/// screen buffer on construction and restores both on drop. Installs
/// a panic hook that restores the terminal before the default hook
/// prints, so a crash inside the render task does not leave the
/// shell in raw mode.
struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Mouse capture lets us receive wheel events for conversation
        // scroll. Terminals honour `Shift`+drag to bypass capture and
        // restore native text selection; per skill anti-pattern #6
        // that is the expected contract.
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

        // TODO(panic-hook): `set_hook` is process-global. If a future
        // design allows multiple TerminalGuards to coexist, only the
        // first-installed hook will restore terminal state. Fine for
        // MS0 — one TUI channel per process.
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            original(info);
        }));

        Ok(Self { _private: () })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::{CliChannel, CliState, KEY_SCROLL_STEP, Line, Mode, Overlay, apply_outbound};
    use crate::bus::{ChannelID, OutboundPayload};
    use crate::session;
    use crate::skill::SkillIndex;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[test]
    fn queued_inputs_are_fifo() {
        let mut state = CliState::default();

        state.queue_input("first".to_string());
        state.queue_input("second".to_string());

        assert_eq!(state.pop_next_queued_input().as_deref(), Some("first"));
        assert_eq!(state.pop_next_queued_input().as_deref(), Some("second"));
        assert!(state.pop_next_queued_input().is_none());
    }

    #[test]
    fn overlay_scroll_resets_when_opening_overlay() {
        let mut state = CliState {
            overlay_scroll_offset: 10,
            ..CliState::default()
        };

        state.open_overlay(Overlay::Help);

        assert_eq!(state.overlay, Some(Overlay::Help));
        assert_eq!(state.overlay_scroll_offset, 0);
    }

    #[test]
    fn overlay_scroll_is_saturating() {
        let mut state = CliState::default();

        state.scroll_overlay_down(5);
        assert_eq!(state.overlay_scroll_offset, 5);

        state.scroll_overlay_up(10);
        assert_eq!(state.overlay_scroll_offset, 0);
    }

    #[test]
    fn transcript_scroll_tracks_arrow_key_steps() {
        let mut state = CliState {
            scroll_offset: 3,
            follow_bottom: true,
            ..CliState::default()
        };

        state.scroll_transcript_up(KEY_SCROLL_STEP);
        assert_eq!(state.scroll_offset, 2);
        assert!(!state.follow_bottom);

        state.scroll_transcript_down(KEY_SCROLL_STEP);
        assert_eq!(state.scroll_offset, 3);
    }

    #[tokio::test]
    async fn overlay_only_consumes_scroll_keys() {
        let dir = std::env::temp_dir().join(format!("mandeven-cli-test-{}", Uuid::now_v7()));
        let sessions = Arc::new(session::Manager::new(dir.clone()).await.unwrap());
        let channel = CliChannel::new(
            ChannelID::new("tui"),
            sessions,
            Arc::new(SkillIndex::new()),
            true,
        );
        channel.state.lock().unwrap().open_overlay(Overlay::Help);

        assert!(
            !channel.handle_overlay_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE,))
        );
        assert!(channel.handle_overlay_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE,)));
        assert_eq!(
            channel.state.lock().unwrap().overlay_scroll_offset,
            KEY_SCROLL_STEP
        );

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[test]
    fn backslash_enter_replaces_backslash_with_newline() {
        let mut state = CliState::default();
        state.input.insert_str("first\\");

        assert!(state.replace_backslash_with_newline());
        assert_eq!(state.input.lines(), ["first", ""]);
    }

    #[test]
    fn clear_input_reports_whether_it_changed_text() {
        let mut state = CliState::default();

        assert!(!state.clear_input());

        state.input.insert_str("draft");
        assert!(state.clear_input());
        assert!(state.input_text().is_empty());
    }

    #[tokio::test]
    async fn reply_end_does_not_end_turn_until_turn_end() {
        let dir = std::env::temp_dir().join(format!("mandeven-cli-test-{}", Uuid::now_v7()));
        let sessions = Arc::new(session::Manager::new(dir.clone()).await.unwrap());
        let state = Arc::new(Mutex::new(CliState {
            mode: Mode::Replying,
            ..CliState::default()
        }));
        let stream_id = Uuid::now_v7();

        apply_outbound(
            &state,
            &sessions,
            OutboundPayload::ReplyDelta {
                stream_id,
                delta: "hello".to_string(),
            },
        )
        .await
        .unwrap();
        apply_outbound(&state, &sessions, OutboundPayload::ReplyEnd { stream_id })
            .await
            .unwrap();

        {
            let state = state.lock().unwrap();
            assert_eq!(state.mode, Mode::Replying);
            assert!(
                matches!(state.transcript.last(), Some(Line::Assistant(text)) if text == "hello")
            );
        }

        apply_outbound(&state, &sessions, OutboundPayload::TurnEnd)
            .await
            .unwrap();

        assert_eq!(state.lock().unwrap().mode, Mode::Idle);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
