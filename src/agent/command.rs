//! Agent-level slash-command handlers.
//!
//! Parsing lives in [`crate::command::slash`]. This module receives typed
//! command enums and applies them to agent-owned state.

use std::fmt::Write as _;
use std::sync::{Arc, RwLock};

use super::compact::CompactReport;
use crate::bus::{ChannelID, OutboundMessage, OutboundPayload, OutboundSender, SessionID};
use crate::channels::discord::DiscordControl;
use crate::channels::wechat::WechatControl;
use crate::command::CommandOutcome;
use crate::command::slash::{
    DiscordCommand as ParsedDiscordCommand, MemoryCommand as ParsedMemoryCommand,
    WechatCommand as ParsedWechatCommand,
};
use crate::config::AppConfig;
use crate::memory::{self, Memory, MemoryQuery, UserProfile};

/// Execution context for agent-level commands.
pub struct AgentCommandCtx {
    /// Channel that originated the command; used by the agent loop
    /// to address the outbound reply.
    pub channel: ChannelID,
    /// Session the command runs within.
    pub session: SessionID,
    /// Durable memory/profile manager. `/memory` subcommands inspect
    /// and archive records through it.
    pub memory: Arc<memory::Manager>,
    /// Discord allowlist control, present iff the Discord channel is
    /// registered. `/discord` subcommands mutate the runtime allow
    /// list and persist to the JSON sidecar through it.
    pub discord: Option<DiscordControl>,
    /// `WeChat` allowlist/login/control handle, present iff the `WeChat`
    /// channel is registered.
    pub wechat: Option<WechatControl>,
    /// Outbound sender used by long-running commands that need to
    /// surface progress before returning their final outcome (for
    /// example `/wechat login`, which must show the QR code before
    /// waiting for confirmation).
    pub out: OutboundSender,
    /// Live `mandeven.toml` view. `/discord autostart on|off`
    /// mutates `channels.discord.enabled` through this handle and
    /// flushes the file via [`AppConfig::save`] — same lock + same
    /// rollback pattern as `/switch default`.
    pub app_config: Arc<RwLock<AppConfig>>,
}

impl AgentCommandCtx {
    async fn send_notice(&self, text: String) -> crate::bus::Result<()> {
        self.out
            .send(OutboundMessage::new(
                self.channel.clone(),
                self.session.clone(),
                OutboundPayload::Notice(text),
            ))
            .await
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

pub async fn run_wechat_command(
    command: ParsedWechatCommand,
    ctx: &AgentCommandCtx,
) -> CommandOutcome {
    let Some(control) = ctx.wechat.as_ref() else {
        return CommandOutcome::Feedback(
            "wechat channel not configured (add [channels.wechat] to mandeven.toml)".into(),
        );
    };

    match command {
        ParsedWechatCommand::Toggle => toggle_wechat(control).await,
        ParsedWechatCommand::Status => CommandOutcome::Feedback(format_wechat_status(control)),
        ParsedWechatCommand::Login => login_wechat(control, ctx).await,
        ParsedWechatCommand::Logout => match control.logout().await {
            Ok(Some(account_id)) => {
                CommandOutcome::Feedback(format!("wechat: removed saved account {account_id}"))
            }
            Ok(None) => CommandOutcome::Feedback("wechat: no saved account found".into()),
            Err(err) => CommandOutcome::Feedback(format!("logout failed: {err}")),
        },
        ParsedWechatCommand::List => CommandOutcome::Feedback(format_wechat_list(control)),
        ParsedWechatCommand::Allow { user_id } => match control.allow(user_id.clone()).await {
            Ok(true) => CommandOutcome::Feedback(format!("wechat: user {user_id} allowed")),
            Ok(false) => {
                CommandOutcome::Feedback(format!("wechat: user {user_id} already allowed"))
            }
            Err(err) => CommandOutcome::Feedback(format!("allow failed: {err}")),
        },
        ParsedWechatCommand::Deny { user_id } => match control.deny(&user_id).await {
            Ok(true) => CommandOutcome::Feedback(format!("wechat: user {user_id} denied")),
            Ok(false) => {
                CommandOutcome::Feedback(format!("wechat: user {user_id} was not in allow list"))
            }
            Err(err) => CommandOutcome::Feedback(format!("deny failed: {err}")),
        },
        ParsedWechatCommand::Autostart { on } => persist_wechat_autostart(&ctx.app_config, on),
    }
}

async fn toggle_wechat(control: &WechatControl) -> CommandOutcome {
    if control.status().active {
        let _ = control.disable();
        CommandOutcome::Feedback("[INFO] wechat channel stopped".into())
    } else {
        match control.enable().await {
            Ok(_) => CommandOutcome::Feedback("[INFO] wechat channel started".into()),
            Err(err) => CommandOutcome::Feedback(format!("enable failed: {err}")),
        }
    }
}

async fn login_wechat(control: &WechatControl, ctx: &AgentCommandCtx) -> CommandOutcome {
    let login = match control.begin_login().await {
        Ok(login) => login,
        Err(err) => return CommandOutcome::Feedback(format!("wechat login failed: {err}")),
    };
    let notice = format!(
        "wechat login: scan this QR code in WeChat, then confirm on your phone.\n\n{}\n\n{}\nWaiting for confirmation...",
        login.qr_ascii, login.scan_data
    );
    if let Err(err) = ctx.send_notice(notice).await {
        return CommandOutcome::Feedback(format!("failed to display QR code: {err}"));
    }
    match control.finish_login(login).await {
        Ok(creds) => CommandOutcome::Feedback(format!(
            "wechat: login saved account {}. Run /wechat to start the channel.",
            creds.account_id
        )),
        Err(err) => CommandOutcome::Feedback(format!("wechat login failed: {err}")),
    }
}

fn persist_wechat_autostart(app_config: &Arc<RwLock<AppConfig>>, on: bool) -> CommandOutcome {
    let mut cfg = app_config.write().expect("config lock poisoned");
    let Some(wechat) = cfg.channels.wechat.as_mut() else {
        return CommandOutcome::Feedback(
            "wechat channel not configured (add [channels.wechat] to mandeven.toml)".into(),
        );
    };
    if wechat.enabled == on {
        let state = if on { "on" } else { "off" };
        return CommandOutcome::Feedback(format!("wechat: autostart already {state}"));
    }
    let previous = wechat.enabled;
    wechat.enabled = on;
    if let Err(err) = cfg.save() {
        if let Some(w) = cfg.channels.wechat.as_mut() {
            w.enabled = previous;
        }
        return CommandOutcome::Feedback(format!("autostart persist failed: {err}"));
    }
    let state = if on { "on" } else { "off" };
    CommandOutcome::Feedback(format!("wechat: autostart {state} (mandeven.toml updated)"))
}

fn format_wechat_status(control: &WechatControl) -> String {
    let s = control.status();
    let state = if s.active { "enabled" } else { "disabled" };
    let account = s.account_id.as_deref().unwrap_or("not staged");
    format!(
        "wechat: {state} · account: {account} · allow list: {} user(s)",
        s.allowed_count
    )
}

fn format_wechat_list(control: &WechatControl) -> String {
    let ids = control.list();
    if ids.is_empty() {
        return "wechat: allow list is empty (no one can DM the bot)".to_string();
    }
    let mut out = format!("wechat: {} allowed user(s)", ids.len());
    for id in ids {
        let _ = write!(out, "\n  {id}");
    }
    out
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

/// One-line snapshot rendered by `/discord` (no args).
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

pub async fn run_memory_command(
    command: ParsedMemoryCommand,
    ctx: &AgentCommandCtx,
) -> CommandOutcome {
    match command {
        ParsedMemoryCommand::List => match ctx
            .memory
            .search(MemoryQuery {
                limit: Some(20),
                ..MemoryQuery::default()
            })
            .await
        {
            Ok(matches) => CommandOutcome::Feedback(format_memory_list(
                matches.into_iter().map(|item| item.memory).collect(),
            )),
            Err(err) => CommandOutcome::Feedback(format!("memory list failed: {err}")),
        },
        ParsedMemoryCommand::Search { query } => match ctx
            .memory
            .search(MemoryQuery {
                query: Some(query.clone()),
                limit: Some(20),
                ..MemoryQuery::default()
            })
            .await
        {
            Ok(matches) => CommandOutcome::Feedback(format_memory_list(
                matches.into_iter().map(|item| item.memory).collect(),
            )),
            Err(err) => CommandOutcome::Feedback(format!("memory search failed: {err}")),
        },
        ParsedMemoryCommand::Show { id } => match ctx.memory.get(&id).await {
            Ok(Some(memory)) => CommandOutcome::Feedback(format_memory_detail(&memory)),
            Ok(None) => CommandOutcome::Feedback(format!("memory {id:?} not found")),
            Err(err) => CommandOutcome::Feedback(format!("memory show failed: {err}")),
        },
        ParsedMemoryCommand::Forget { id } => match ctx.memory.forget(&id).await {
            Ok(Some(memory)) => CommandOutcome::Feedback(format!(
                "memory {} archived: {}",
                memory::short_id(&memory.id),
                memory.title
            )),
            Ok(None) => CommandOutcome::Feedback(format!("memory {id:?} not found")),
            Err(err) => CommandOutcome::Feedback(format!("memory forget failed: {err}")),
        },
        ParsedMemoryCommand::Profile => match ctx.memory.profile().await {
            Ok(profile) => CommandOutcome::Feedback(format_profile(&profile)),
            Err(err) => CommandOutcome::Feedback(format!("memory profile failed: {err}")),
        },
    }
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

fn format_memory_list(memories: Vec<Memory>) -> String {
    if memories.is_empty() {
        return "memory: no active memories".to_string();
    }
    let mut out = format!("memory: {} active record(s)", memories.len());
    for memory in memories {
        let _ = write!(
            out,
            "\n  {} [{}/{}] {} — {}",
            memory::short_id(&memory.id),
            memory::scope_name(memory.scope),
            memory::kind_name(memory.kind),
            truncate(&memory.title, 36),
            truncate(&memory.summary, 72)
        );
    }
    out
}

fn format_memory_detail(memory: &Memory) -> String {
    let mut out = format!(
        "memory {} [{}/{}/{}]\n{}\n\n{}",
        memory::short_id(&memory.id),
        memory::scope_name(memory.scope),
        memory::kind_name(memory.kind),
        memory::status_name(memory.status),
        memory.title,
        memory.body
    );
    if !memory.tags.is_empty() {
        let _ = write!(out, "\n\ntags: {}", memory.tags.join(", "));
    }
    let _ = write!(
        out,
        "\ncreated: {}\nupdated: {}",
        memory.created_at.to_rfc3339(),
        memory.updated_at.to_rfc3339()
    );
    out
}

fn format_profile(profile: &UserProfile) -> String {
    if profile.summary.is_empty()
        && profile.communication_style.is_empty()
        && profile.working_preferences.is_empty()
        && profile.avoid.is_empty()
    {
        return "memory profile: empty".to_string();
    }
    let mut out = String::from("memory profile");
    if !profile.summary.is_empty() {
        let _ = write!(out, "\nsummary: {}", profile.summary);
    }
    write_profile_items(&mut out, "communication", &profile.communication_style);
    write_profile_items(&mut out, "work", &profile.working_preferences);
    write_profile_items(&mut out, "avoid", &profile.avoid);
    if !profile.source_memory_ids.is_empty() {
        let ids = profile
            .source_memory_ids
            .iter()
            .map(|id| memory::short_id(id))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(out, "\nsources: {ids}");
    }
    out
}

fn write_profile_items(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = write!(out, "\n{label}: {}", items.join("; "));
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
