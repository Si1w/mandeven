//! Agent-level slash-command handlers.
//!
//! Parsing lives in [`crate::command::slash`]. This module receives typed
//! command enums and applies them to agent-owned state.

use std::sync::Arc;

use super::compact::CompactReport;
use crate::bus::{ChannelID, SessionID};
use crate::command::CommandOutcome;
use crate::command::slash::{
    CronCommand as ParsedCronCommand, HeartbeatCommand as ParsedHeartbeatCommand,
};
use crate::cron::{CronEngine, CronJob, CronStatus, RunStatus};
use crate::heartbeat::{HeartbeatEngine, HeartbeatStatus};

/// Execution context for agent-level commands.
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

pub async fn run_heartbeat_command(
    command: ParsedHeartbeatCommand,
    ctx: &AgentCommandCtx,
) -> CommandOutcome {
    let Some(engine) = ctx.heartbeat.as_ref() else {
        return CommandOutcome::Feedback(
            "heartbeat is not configured (set agent.heartbeat.enabled = true to enable)".into(),
        );
    };

    match command {
        ParsedHeartbeatCommand::Status => CommandOutcome::Feedback(format_status(&engine.status())),
        ParsedHeartbeatCommand::Pause => {
            engine.pause();
            CommandOutcome::Feedback("heartbeat paused".into())
        }
        ParsedHeartbeatCommand::Resume => {
            engine.resume();
            CommandOutcome::Feedback("heartbeat resumed".into())
        }
        ParsedHeartbeatCommand::Trigger => {
            engine.trigger();
            CommandOutcome::Feedback("heartbeat trigger requested".into())
        }
        ParsedHeartbeatCommand::Interval { seconds: 0 } => {
            CommandOutcome::Feedback("interval must be > 0".into())
        }
        ParsedHeartbeatCommand::Interval { seconds } => {
            engine.set_interval(seconds);
            CommandOutcome::Feedback(format!("heartbeat interval set to {seconds}s"))
        }
    }
}

pub async fn run_cron_command(command: ParsedCronCommand, ctx: &AgentCommandCtx) -> CommandOutcome {
    let Some(engine) = ctx.cron.as_ref() else {
        return CommandOutcome::Feedback(
            "cron is not configured (set agent.cron.enabled = true to enable)".into(),
        );
    };

    match command {
        ParsedCronCommand::List => {
            CommandOutcome::Feedback(format_cron_status(&engine.status().await))
        }
        ParsedCronCommand::Trigger { id } => match engine.trigger(&id).await {
            Ok(()) => CommandOutcome::Feedback(format!("cron job {id:?} trigger requested")),
            Err(err) => CommandOutcome::Feedback(format!("trigger failed: {err}")),
        },
        ParsedCronCommand::Enable { id } => match engine.set_enabled(&id, true).await {
            Ok(()) => CommandOutcome::Feedback(format!("cron job {id:?} enabled")),
            Err(err) => CommandOutcome::Feedback(format!("enable failed: {err}")),
        },
        ParsedCronCommand::Disable { id } => match engine.set_enabled(&id, false).await {
            Ok(()) => CommandOutcome::Feedback(format!("cron job {id:?} disabled")),
            Err(err) => CommandOutcome::Feedback(format!("disable failed: {err}")),
        },
        ParsedCronCommand::Remove { id } => match engine.remove(&id).await {
            Ok(()) => CommandOutcome::Feedback(format!("cron job {id:?} removed")),
            Err(err) => CommandOutcome::Feedback(format!("remove failed: {err}")),
        },
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
/// prefix and renders next/last timestamps in compact ISO form.
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
        Some(RunStatus::Succeeded) => "ok",
        Some(RunStatus::Failed) => "err",
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
