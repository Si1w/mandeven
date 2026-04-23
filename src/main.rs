//! Minimal end-to-end demo: crossterm-driven stdin REPL over a single
//! session.
//!
//! FIXME: throwaway test harness. This file exists only to exercise
//! the agent ↔ session ↔ LLM path end-to-end before real channels
//! (`cli/`, `tui/`) and command dispatch (`command/`) come online.
//! Delete — do not extend — once those land; the production `main`
//! will wire `channels::Dispatcher` (or equivalent) instead of
//! inlining a REPL here.
//!
//! Raw mode + [`crossterm::event::EventStream`] drives input; the
//! [`tokio::select!`] loop simultaneously consumes user keystrokes and
//! outbound messages from the agent. Requires `MISTRAL_API_KEY` in the
//! environment and `./mandeven.toml` in the working directory.
//!
//! The UX deliberately omits history, cursor movement, and typing
//! during streaming replies — those belong to the future ratatui-based
//! TUI, not this bootstrap demo.

use std::io::{self, Write};
use std::sync::Arc;

use mandeven::agent::Agent;
use mandeven::bus::{
    Bus, ChannelID, InboundMessage, InboundPayload, OutboundPayload, OutboundReceiver, Sender,
    SessionID,
};
use mandeven::config::AppConfig;
use mandeven::session;
use mandeven::tools;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;

/// Directory under the config's `data_dir` where session files live.
const SESSION_SUBDIR: &str = "sessions";

/// Channel identifier this demo announces itself as.
const CLI_CHANNEL: &str = "cli";

/// ANSI sequence: carriage return + erase entire line. Used to
/// repaint the prompt line after any edit.
const LINE_RESET: &str = "\r\x1b[2K";

/// REPL prompt shown before the input buffer.
const PROMPT: &str = "> ";

/// Boxed error alias used at the `main` boundary.
type DynError = Box<dyn std::error::Error + Send + Sync>;

/// Raw-mode RAII guard: ensures [`disable_raw_mode`] runs on every
/// exit path, including panic.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Where the REPL is in its per-turn state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Accepting user input.
    Idle,
    /// A user turn is in flight; keystrokes other than Ctrl-C/D are
    /// ignored until the agent signals completion.
    Replying,
}

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let cfg = AppConfig::load()?;
    let sessions = Arc::new(session::Manager::new(cfg.data_dir().join(SESSION_SUBDIR)).await?);

    let (bus, inbound_rx, outbound_rx) = Bus::new();
    let inbound = bus.inbound_sender();
    let outbound = bus.outbound_sender();
    drop(bus);

    let mut tools = tools::Registry::new();
    tools::register_builtins(&mut tools);
    let agent = Agent::new(&cfg, sessions, tools, inbound_rx, outbound)?;
    let agent_handle = tokio::spawn(agent.run());

    let channel = ChannelID::new(CLI_CHANNEL);
    let session = SessionID::new();

    let guard = RawModeGuard::enter()?;
    let repl_result = run_repl(&inbound, outbound_rx, &channel, &session).await;
    drop(guard);

    drop(inbound);
    let agent_result = agent_handle.await?;

    repl_result?;
    agent_result?;
    Ok(())
}

/// Drive the REPL loop until Ctrl-C/D, EOF on stdin events, or the
/// bus closes.
async fn run_repl(
    inbound: &Sender<InboundMessage>,
    mut outbound_rx: OutboundReceiver,
    channel: &ChannelID,
    session: &SessionID,
) -> Result<(), DynError> {
    let mut input = String::new();
    let mut state = State::Idle;
    let mut events = EventStream::new();

    redraw_prompt(&input)?;

    loop {
        tokio::select! {
            maybe_event = events.next() => {
                let Some(event) = maybe_event else { break; };
                if !handle_event(event?, &mut input, &mut state, inbound, channel, session).await? {
                    break;
                }
            }
            maybe_msg = outbound_rx.recv() => {
                let Some(msg) = maybe_msg else { break; };
                handle_outbound(msg.payload, &input, &mut state)?;
            }
        }
    }

    Ok(())
}

/// Apply one terminal event. Returns `Ok(false)` when the REPL should
/// exit.
async fn handle_event(
    event: Event,
    input: &mut String,
    state: &mut State,
    inbound: &Sender<InboundMessage>,
    channel: &ChannelID,
    session: &SessionID,
) -> Result<bool, DynError> {
    let Event::Key(KeyEvent {
        code, modifiers, ..
    }) = event
    else {
        return Ok(true);
    };

    // Ctrl-C / Ctrl-D always exit, regardless of state.
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'd')) {
        print!("\r\n");
        io::stdout().flush()?;
        return Ok(false);
    }

    // While replying, swallow everything else until the turn finishes.
    if *state == State::Replying {
        return Ok(true);
    }

    match code {
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            input.push(c);
            redraw_prompt(input)?;
        }
        KeyCode::Backspace => {
            if input.pop().is_some() {
                redraw_prompt(input)?;
            }
        }
        KeyCode::Enter => {
            print!("\r\n");
            io::stdout().flush()?;
            let line = std::mem::take(input);
            if line.trim().is_empty() {
                redraw_prompt(input)?;
                return Ok(true);
            }
            let msg = InboundMessage::new(
                channel.clone(),
                session.clone(),
                InboundPayload::UserInput(line),
            );
            if inbound.send(msg).await.is_err() {
                return Ok(false);
            }
            *state = State::Replying;
        }
        _ => {}
    }
    Ok(true)
}

/// Render one outbound message from the agent.
fn handle_outbound(payload: OutboundPayload, input: &str, state: &mut State) -> io::Result<()> {
    let mut stdout = io::stdout();
    match payload {
        OutboundPayload::ReplyDelta { delta, .. } => {
            write_crlf(&mut stdout, &delta)?;
        }
        OutboundPayload::Reply(text) => {
            write_crlf(&mut stdout, &text)?;
            write!(stdout, "\r\n")?;
            *state = State::Idle;
            stdout.flush()?;
            return redraw_prompt(input);
        }
        OutboundPayload::ReplyEnd { .. } => {
            write!(stdout, "\r\n")?;
            *state = State::Idle;
            stdout.flush()?;
            return redraw_prompt(input);
        }
        OutboundPayload::Error(err) => {
            write!(stdout, "\r\n[error] ")?;
            write_crlf(&mut stdout, &err)?;
            write!(stdout, "\r\n")?;
            *state = State::Idle;
            stdout.flush()?;
            return redraw_prompt(input);
        }
    }
    stdout.flush()
}

/// Write LLM-origin text under raw mode, promoting each `\n` into
/// `\r\n` so line breaks both advance and return to column 0.
/// Leaves existing `\r\n` unchanged (the extra `\r` is idempotent).
fn write_crlf(stdout: &mut impl Write, text: &str) -> io::Result<()> {
    if text.contains('\n') {
        stdout.write_all(text.replace('\n', "\r\n").as_bytes())
    } else {
        stdout.write_all(text.as_bytes())
    }
}

/// Repaint the prompt line: return to column 0, erase the line, then
/// write the prompt + current input buffer. Works correctly with
/// multi-byte input because the terminal renders the string as-is.
fn redraw_prompt(input: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "{LINE_RESET}{PROMPT}{input}")?;
    stdout.flush()
}
