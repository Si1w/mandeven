//! Agent-level slash-command handlers.
//!
//! Parsing lives in [`crate::command::slash`]. This module receives typed
//! command enums and applies them to agent-owned state.

use std::sync::{Arc, RwLock};

use super::compact::CompactReport;
use crate::bus::{ChannelID, SessionID};
use crate::channels::discord::DiscordControl;
use crate::command::CommandOutcome;
use crate::command::slash::{
    CronCommand as ParsedCronCommand, DiscordCommand as ParsedDiscordCommand,
    HeartbeatCommand as ParsedHeartbeatCommand,
};
use crate::config::AppConfig;
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
    /// Discord allowlist control, present iff the Discord channel is
    /// registered. `/discord` subcommands mutate the runtime allow
    /// list and persist to the JSON sidecar through it.
    pub discord: Option<DiscordControl>,
    /// Live `mandeven.toml` view. `/discord autostart on|off`
    /// mutates `channels.discord.enabled` through this handle and
    /// flushes the file via [`AppConfig::save`] — same lock + same
    /// rollback pattern as `/switch default`.
    pub app_config: Arc<RwLock<AppConfig>>,
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

pub async fn run_discord_command(
    command: ParsedDiscordCommand,
    ctx: &AgentCommandCtx,
) -> CommandOutcome {
    let Some(control) = ctx.discord.as_ref() else {
        return CommandOutcome::Feedback(
            "discord channel not configured (add [channels.discord] to mandeven.toml)".into(),
        );
    };

    match command {
        ParsedDiscordCommand::Toggle => toggle_discord(control).await,
        ParsedDiscordCommand::Status => CommandOutcome::Feedback(format_discord_status(control)),
        ParsedDiscordCommand::List => CommandOutcome::Feedback(format_discord_list(control)),
        ParsedDiscordCommand::Allow { user_id } => match control.allow(user_id).await {
            Ok(true) => CommandOutcome::Feedback(format!("discord: user {user_id} allowed")),
            Ok(false) => {
                CommandOutcome::Feedback(format!("discord: user {user_id} already allowed"))
            }
            Err(err) => CommandOutcome::Feedback(format!("allow failed: {err}")),
        },
        ParsedDiscordCommand::Deny { user_id } => match control.deny(user_id).await {
            Ok(true) => CommandOutcome::Feedback(format!("discord: user {user_id} denied")),
            Ok(false) => {
                CommandOutcome::Feedback(format!("discord: user {user_id} was not in allow list"))
            }
            Err(err) => CommandOutcome::Feedback(format!("deny failed: {err}")),
        },
        ParsedDiscordCommand::Autostart { on } => persist_discord_autostart(&ctx.app_config, on),
    }
}

/// Flip the Discord gateway connection. Reads the current desired
/// state and inverts it; the response reports the new state so the
/// user sees the result without a separate `/discord status` call.
async fn toggle_discord(control: &DiscordControl) -> CommandOutcome {
    if control.status().active {
        let _ = control.disable();
        CommandOutcome::Feedback("[INFO] discord channel stopped".into())
    } else {
        match control.enable().await {
            Ok(_) => CommandOutcome::Feedback("[INFO] discord channel started".into()),
            Err(err) => CommandOutcome::Feedback(format!("enable failed: {err}")),
        }
    }
}

/// Mutate `[channels.discord].enabled` and atomically rewrite
/// `mandeven.toml`. Mirrors the rollback discipline of
/// [`super::Agent::switch_default_model`]: revert the in-memory edit
/// when [`AppConfig::save`] fails so config-on-disk and the cached
/// view stay in sync.
fn persist_discord_autostart(app_config: &Arc<RwLock<AppConfig>>, on: bool) -> CommandOutcome {
    let mut cfg = app_config.write().expect("config lock poisoned");
    let Some(discord) = cfg.channels.discord.as_mut() else {
        return CommandOutcome::Feedback(
            "discord channel not configured (add [channels.discord] to mandeven.toml)".into(),
        );
    };
    if discord.enabled == on {
        let state = if on { "on" } else { "off" };
        return CommandOutcome::Feedback(format!("discord: autostart already {state}"));
    }
    let previous = discord.enabled;
    discord.enabled = on;
    if let Err(err) = cfg.save() {
        if let Some(d) = cfg.channels.discord.as_mut() {
            d.enabled = previous;
        }
        return CommandOutcome::Feedback(format!("autostart persist failed: {err}"));
    }
    let state = if on { "on" } else { "off" };
    CommandOutcome::Feedback(format!(
        "discord: autostart {state} (mandeven.toml updated)"
    ))
}

/// One-line snapshot rendered by `/discord` (no args). Mirrors the
/// `/heartbeat` style.
fn format_discord_status(control: &DiscordControl) -> String {
    let s = control.status();
    let state = if s.active { "enabled" } else { "disabled" };
    format!("discord: {state} · allow list: {} user(s)", s.allowed_count)
}

/// Render the current Discord allow list as a multi-line block, one
/// id per line. Header summarizes count.
fn format_discord_list(control: &DiscordControl) -> String {
    use std::fmt::Write as _;

    let ids = control.list();
    if ids.is_empty() {
        return "discord: allow list is empty (no one can DM the bot)".to_string();
    }
    let mut out = format!("discord: {} allowed user(s)", ids.len());
    for id in ids {
        let _ = write!(out, "\n  {id}");
    }
    out
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
