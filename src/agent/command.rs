//! Agent-level slash commands — operate on agent-internal state
//! (heartbeat controls, `/compact`; `/stop`, `/status`, … to come).
//! Routing / session-level commands (`/new`, `/list`, `/load`) live
//! in [`crate::gateway::commands`] instead.
//!
//! Most commands plug into the trait-based router used elsewhere
//! ([`crate::command::Command`]). `/compact` is the exception: its
//! work needs the agent's `&self` (LLM client, session store) and
//! is async, so it is special-cased on the
//! [`super::Agent::dispatch_command`] path. The parsing helper
//! [`parse_compact_command`] sits here so all command logic lives
//! together.

use std::sync::Arc;

use async_trait::async_trait;

use super::compact::CompactReport;
use crate::bus::{ChannelID, SessionID};
use crate::command::{Command, CommandOutcome};
use crate::cron::{CronEngine, CronJob, CronStatus, RunStatus};
use crate::heartbeat::{HeartbeatEngine, HeartbeatStatus};

/// Execution context for agent-level commands.
///
/// Kept intentionally small; commands that need richer handles
/// (active task list, current iteration, etc.) extend this struct
/// when they land. Named with `CommandCtx` rather than bare
/// `Context` to avoid colliding with the conversation-history
/// sense of "context" elsewhere in the codebase.
pub struct AgentCommandCtx {
    /// Channel that originated the command; used by the agent loop
    /// to address the outbound reply.
    pub channel: ChannelID,
    /// Session the command runs within.
    pub session: SessionID,
    /// Heartbeat engine handle, present iff the agent has heartbeat
    /// enabled. `/heartbeat` subcommands flip flags through this.
    pub heartbeat: Option<Arc<HeartbeatEngine>>,
    /// Cron engine handle, present iff the agent has cron enabled.
    /// `/cron` subcommands list, trigger, and pause jobs through it.
    pub cron: Option<Arc<CronEngine>>,
}

/// `/heartbeat` — status (no args) plus pause / resume / trigger /
/// interval `<secs>` subcommands. Operates on
/// [`AgentCommandCtx::heartbeat`]; reports a friendly notice when
/// the engine is absent (heartbeat was disabled in config).
pub struct Heartbeat;

#[async_trait]
impl Command<AgentCommandCtx> for Heartbeat {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    fn describe(&self) -> &'static str {
        "show or control the heartbeat engine — subcmds: pause, resume, trigger, interval <secs>"
    }

    async fn execute(&self, args: &str, ctx: &AgentCommandCtx) -> CommandOutcome {
        let Some(engine) = ctx.heartbeat.as_ref() else {
            return CommandOutcome::Feedback(
                "heartbeat is not configured (set agent.heartbeat.enabled = true to enable)".into(),
            );
        };

        let trimmed = args.trim();
        let (sub, rest) = trimmed
            .split_once(char::is_whitespace)
            .map_or((trimmed, ""), |(s, r)| (s, r.trim()));

        match sub {
            "" => CommandOutcome::Feedback(format_status(&engine.status())),
            "pause" => {
                engine.pause();
                CommandOutcome::Feedback("heartbeat paused".into())
            }
            "resume" => {
                engine.resume();
                CommandOutcome::Feedback("heartbeat resumed".into())
            }
            "trigger" => {
                engine.trigger();
                CommandOutcome::Feedback("heartbeat trigger requested".into())
            }
            "interval" => match rest.parse::<u64>() {
                Ok(0) => CommandOutcome::Feedback("interval must be > 0".into()),
                Ok(secs) => {
                    engine.set_interval(secs);
                    CommandOutcome::Feedback(format!("heartbeat interval set to {secs}s"))
                }
                Err(_) => CommandOutcome::Feedback(format!(
                    "usage: /heartbeat interval <seconds>; got {rest:?}"
                )),
            },
            other => CommandOutcome::Feedback(format!(
                "unknown subcommand {other:?}; valid: pause, resume, trigger, interval <secs>"
            )),
        }
    }
}

/// `/cron` — list jobs (no args) or run a per-job subcommand
/// (`trigger` / `enable` / `disable` / `remove`).
///
/// All subcommands take the firing job's full id; partial matching
/// would invite ambiguity for very little ergonomic gain at this size.
pub struct Cron;

#[async_trait]
impl Command<AgentCommandCtx> for Cron {
    fn name(&self) -> &'static str {
        "cron"
    }

    fn describe(&self) -> &'static str {
        "list cron jobs or run subcmds: trigger <id>, enable <id>, disable <id>, remove <id>"
    }

    async fn execute(&self, args: &str, ctx: &AgentCommandCtx) -> CommandOutcome {
        let Some(engine) = ctx.cron.as_ref() else {
            return CommandOutcome::Feedback(
                "cron is not configured (set agent.cron.enabled = true to enable)".into(),
            );
        };

        let trimmed = args.trim();
        let (sub, rest) = trimmed
            .split_once(char::is_whitespace)
            .map_or((trimmed, ""), |(s, r)| (s, r.trim()));

        match sub {
            "" => CommandOutcome::Feedback(format_cron_status(&engine.status().await)),
            "trigger" => match engine.trigger(rest).await {
                Ok(()) => CommandOutcome::Feedback(format!("cron job {rest:?} trigger requested")),
                Err(err) => CommandOutcome::Feedback(format!("trigger failed: {err}")),
            },
            "enable" => match engine.set_enabled(rest, true).await {
                Ok(()) => CommandOutcome::Feedback(format!("cron job {rest:?} enabled")),
                Err(err) => CommandOutcome::Feedback(format!("enable failed: {err}")),
            },
            "disable" => match engine.set_enabled(rest, false).await {
                Ok(()) => CommandOutcome::Feedback(format!("cron job {rest:?} disabled")),
                Err(err) => CommandOutcome::Feedback(format!("disable failed: {err}")),
            },
            "remove" => match engine.remove(rest).await {
                Ok(()) => CommandOutcome::Feedback(format!("cron job {rest:?} removed")),
                Err(err) => CommandOutcome::Feedback(format!("remove failed: {err}")),
            },
            other => CommandOutcome::Feedback(format!(
                "unknown subcommand {other:?}; valid: trigger, enable, disable, remove"
            )),
        }
    }
}

/// Multi-line status block rendered by `/cron` (no args). Header
/// summarizes engine state; one line per job follows.
fn format_cron_status(status: &CronStatus) -> String {
    let header = if status.enabled {
        format!("cron: enabled · {} jobs", status.jobs.len())
    } else {
        format!("cron: disabled · {} jobs persisted", status.jobs.len())
    };
    if status.jobs.is_empty() {
        return header;
    }
    let lines = status.jobs.iter().map(format_job_line).collect::<Vec<_>>();
    format!("{header}\n{}", lines.join("\n"))
}

/// One job's line in `/cron` output. Truncates the id to its UUID v7
/// prefix (the leading 8 chars are plenty unique within a single
/// user's job set) and renders next/last timestamps in compact ISO
/// form.
fn format_job_line(job: &CronJob) -> String {
    let state = if job.enabled { "on " } else { "off" };
    let short_id = job.id.split('-').next().unwrap_or(job.id.as_str());
    let next = job.state.next_run_at.map_or_else(
        || "next=never".to_string(),
        |t| format!("next={}", t.to_rfc3339()),
    );
    let last = job.state.last_run_at.map_or_else(
        || "last=never".to_string(),
        |t| format!("last={}", t.to_rfc3339()),
    );
    let status_tag = match job.state.last_status {
        Some(RunStatus::Ok) => "ok",
        Some(RunStatus::Error) => "err",
        Some(RunStatus::Skipped) => "skip",
        None => "—",
    };
    format!(
        "  [{state}] {short_id} {name:<24} {sched:<22} {next} {last} {status_tag} errs={errs}",
        name = truncate(&job.name, 24),
        sched = truncate(&job.schedule.describe(), 22),
        errs = job.state.consecutive_errors,
    )
}

/// Truncate a string to `max` chars, replacing the tail with `…` when
/// it overflows.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// One-line status summary rendered by `/heartbeat` (no args).
fn format_status(status: &HeartbeatStatus) -> String {
    let state = if !status.enabled {
        "disabled"
    } else if status.paused {
        "paused"
    } else {
        "active"
    };
    let last = status
        .last_tick_at
        .map_or_else(|| "never".to_string(), |t| t.format("%H:%M:%S").to_string());
    let next = status
        .next_tick_in_secs
        .map_or_else(|| "n/a".to_string(), |s| format!("{s}s"));
    format!(
        "heartbeat: {state} · interval={}s · last_tick={last} · next_tick_in={next}",
        status.interval_secs
    )
}

/// Outcome of [`parse_compact_command`].
///
/// Used in lieu of `Option<Option<String>>` so the three states are
/// named at the call site.
#[derive(Debug)]
pub enum CompactCmdMatch {
    /// Body wasn't a `/compact` request — caller falls through to
    /// the regular trait-based router.
    None,
    /// `/compact` with no focus argument.
    Bare,
    /// `/compact <focus>` with non-empty focus text.
    Focused(String),
}

/// Recognize `/compact` and `/compact <focus...>` command bodies.
///
/// `body` is the trimmed substring after the leading `/`. Anything
/// that isn't a compact request returns [`CompactCmdMatch::None`].
#[must_use]
pub fn parse_compact_command(body: &str) -> CompactCmdMatch {
    let trimmed = body.trim();
    if trimmed == "compact" {
        return CompactCmdMatch::Bare;
    }
    let Some(rest) = trimmed.strip_prefix("compact") else {
        return CompactCmdMatch::None;
    };
    if !rest.starts_with(char::is_whitespace) {
        return CompactCmdMatch::None;
    }
    let focus = rest.trim();
    if focus.is_empty() {
        CompactCmdMatch::Bare
    } else {
        CompactCmdMatch::Focused(focus.to_string())
    }
}

/// One-line success summary rendered to the user after `/compact`.
#[must_use]
pub fn format_compact_report(report: &CompactReport) -> String {
    format!(
        "compacted {} → {} messages (≈{} → {} tokens)",
        report.messages_before,
        report.messages_after,
        report.estimated_tokens_before,
        report.estimated_tokens_after,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compact_recognizes_bare_and_focused_forms() {
        assert!(matches!(
            parse_compact_command("compact"),
            CompactCmdMatch::Bare
        ));
        assert!(matches!(
            parse_compact_command("compact   "),
            CompactCmdMatch::Bare
        ));
        match parse_compact_command("compact recent file edits") {
            CompactCmdMatch::Focused(s) => assert_eq!(s, "recent file edits"),
            other => panic!("expected Focused, got {other:?}"),
        }
    }

    #[test]
    fn parse_compact_rejects_non_compact_bodies() {
        assert!(matches!(
            parse_compact_command("help"),
            CompactCmdMatch::None
        ));
        // "compactor" must NOT match — prefix-only collision.
        assert!(matches!(
            parse_compact_command("compactor"),
            CompactCmdMatch::None
        ));
        assert!(matches!(parse_compact_command(""), CompactCmdMatch::None));
    }
}
