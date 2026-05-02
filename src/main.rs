//! Mandeven — terminal agent bootstrap.
//!
//! Wires the domain modules together:
//! [`bus`](mandeven::bus) for in-process messaging,
//! [`gateway`](mandeven::gateway) for session routing,
//! [`agent`](mandeven::agent) for the iteration loop,
//! [`channels`](mandeven::channels) for the channel registry + router,
//! and [`cli`](mandeven::cli) as the currently-registered TUI channel.
//! Requires the configured provider's API key in the environment.
//! Configuration is loaded from `~/.mandeven/mandeven.toml` (or the
//! path under `$MANDEVEN_HOME`) and created interactively on first run.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use mandeven::agent::{Agent, DiscordWiring, TimerWiring, WechatWiring};
use mandeven::bus::{Bus, ChannelID};
use mandeven::channels::Manager;
use mandeven::channels::discord::{self, DiscordChannel};
use mandeven::channels::wechat::{self, WechatChannel};
use mandeven::cli::CliChannel;
use mandeven::config::{self, AppConfig};
use mandeven::exec;
use mandeven::gateway::{Gateway, dispatch_channel};
use mandeven::hook::HookEngine;
use mandeven::memory;
use mandeven::prompt::PromptEngine;
use mandeven::security::SandboxPolicy;
use mandeven::session;
use mandeven::skill::{self, SkillIndex};
use mandeven::task;
use mandeven::timer;
use mandeven::tools;
use mandeven::utils::workspace;

/// Identifier for the built-in TUI channel.
const TUI_CHANNEL: &str = "tui";

/// Identifier for the Discord channel adapter.
const DISCORD_CHANNEL: &str = "discord";

/// Identifier for the `WeChat` channel adapter.
const WECHAT_CHANNEL: &str = "wechat";

/// Boxed error alias used at the `main` boundary.
type DynError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), DynError> {
    let cfg = AppConfig::bootstrap()?;

    // Capture the launch CWD once. The canonical form anchors the
    // workspace boundary every tool reads via `workspace::root()`; the
    // raw form drives the per-project bucket.
    let cwd = std::env::current_dir()?;
    let canonical_cwd = std::fs::canonicalize(&cwd)?;
    workspace::init(canonical_cwd);

    // Install the sandbox tier before any tool is registered. Tools read
    // it via `SandboxPolicy::current()` on each invocation; missing
    // `[sandbox]` block in the TOML keeps the default `WorkspaceWrite`.
    SandboxPolicy::init(cfg.sandbox.policy);

    let project_bucket = config::project_bucket(&cwd);

    let sessions = Arc::new(session::Manager::new(project_bucket.clone()).await?);
    let cron_sessions = Arc::new(session::Manager::new(config::cron_bucket()).await?);

    // Skill index reads ~/.mandeven/skills/<name>/SKILL.md once at
    // boot. Disabled => empty index, no SkillTool registration, no
    // skills_index section in the prompt.
    let skill_index = if cfg.agent.skill.enabled {
        skill::seed_builtins(&cfg.data_dir())?;
        skill::load(&cfg.data_dir().join(skill::SKILLS_SUBDIR))?
    } else {
        SkillIndex::new()
    };
    if cfg.agent.skill.enabled {
        timer::sync_skill_timers(&cfg.data_dir(), &skill_index).await?;
    }
    let skill_index = Arc::new(skill_index);

    // Prompt engine reads ~/.mandeven/AGENTS.md once at boot and
    // borrows the skill index for the skills_index section. The
    // section cache fills lazily as iteration_system is called.
    let prompts = Arc::new(PromptEngine::load(&cfg.data_dir(), &skill_index)?);

    // Hook engine reads ~/.mandeven/hooks.json once at boot. When
    // the file is absent or `[agent.hook] enabled = false`, the
    // engine becomes a no-op — every fire() returns immediately
    // without spawning anything.
    let hooks = Arc::new(HookEngine::load(cfg.agent.hook.enabled, &cfg.data_dir())?);

    // Three queues:
    //   channels → gateway  (InboundMessage, identity-only)
    //   gateway  → agent    (InboundDispatch, session attached)
    //   agent + gateway → channels  (OutboundMessage)
    let (bus, inbound_rx, outbound_rx) = Bus::new();
    let inbound_tx = bus.inbound_sender();
    let outbound_tx = bus.outbound_sender();
    drop(bus);
    let (dispatch_tx, dispatch_rx) = dispatch_channel();

    // Shared per-channel session map: gateway is the writer, the
    // agent reads it when a background timer needs to notify the
    // currently active TUI session.
    let active_sessions = Arc::new(Mutex::new(HashMap::new()));

    let (engine, rx) = timer::TimerEngine::new(&project_bucket, &cfg.data_dir()).await?;
    let engine = Arc::new(engine);
    engine.start().await;
    let timer_wiring = Some(TimerWiring { engine, rx });

    let memory_manager = Arc::new(memory::Manager::new(&cfg.data_dir()));
    memory_manager.ensure_exists(&cfg.agent.memory).await?;
    let task_manager = Arc::new(task::Manager::new(&project_bucket));
    let exec_manager = Arc::new(exec::Manager::new(&project_bucket));

    let tool_registry = build_tool_registry(
        &cfg.data_dir(),
        &project_bucket,
        &skill_index,
        task_manager.clone(),
    );

    let (discord_channel, discord_wiring) = build_discord(&cfg).await?;
    let discord_active_rx = discord_wiring
        .as_ref()
        .map(|w| w.control.subscribe_active());
    let (wechat_channel, wechat_wiring) = build_wechat(&cfg).await?;
    let wechat_active_rx = wechat_wiring.as_ref().map(|w| w.control.subscribe_active());

    let agent = Agent::new(
        &cfg,
        sessions.clone(),
        cron_sessions,
        tool_registry,
        dispatch_rx,
        outbound_tx.clone(),
        active_sessions.clone(),
        timer_wiring,
        memory_manager,
        task_manager,
        skill_index.clone(),
        exec_manager,
        discord_wiring,
        wechat_wiring,
        prompts,
        hooks,
        cwd,
    )?;

    let gateway = Gateway::new(
        inbound_rx,
        dispatch_tx,
        outbound_tx,
        sessions.clone(),
        active_sessions,
    );

    let mut manager = Manager::new(outbound_rx);
    manager.register(Arc::new(CliChannel::new(
        ChannelID::new(TUI_CHANNEL),
        sessions,
        skill_index,
        cfg.tui.show_thinking,
        discord_active_rx,
        wechat_active_rx,
    )));

    if let Some(channel) = discord_channel {
        manager.register(Arc::new(channel));
    }
    if let Some(channel) = wechat_channel {
        manager.register(Arc::new(channel));
    }

    let agent_handle = tokio::spawn(agent.run());
    let gateway_handle = tokio::spawn(gateway.run());
    manager.run(inbound_tx).await?;
    agent_handle.await??;
    gateway_handle.await??;
    Ok(())
}

fn build_tool_registry(
    data_dir: &Path,
    project_bucket: &Path,
    skill_index: &Arc<SkillIndex>,
    task_manager: Arc<task::Manager>,
) -> tools::Registry {
    let mut registry = tools::Registry::new();
    tools::register_builtins(&mut registry);
    tools::task::register(&mut registry, task_manager);
    tools::timer::register(
        &mut registry,
        Arc::new(timer::Manager::new(data_dir, project_bucket)),
    );
    if !skill_index.is_empty() {
        registry.register(Arc::new(tools::skill::SkillTool::new(skill_index.clone())));
    }
    registry
}

/// Resolve the Discord channel + control pair from config.
///
/// Returns `(None, None)` when `[channels.discord]` is absent (the
/// user has not opted into Discord at all). When the section exists
/// the channel is **always** built — `enabled = true` only triggers
/// an auto-enable here so the connection is open at boot. The user
/// can flip the gateway connection at runtime via `/discord
/// enable|disable` in either case.
///
/// Pulled out of `main` to keep the bootstrap sequence linear and
/// stay under the `too_many_lines` lint threshold.
async fn build_discord(
    cfg: &AppConfig,
) -> Result<(Option<DiscordChannel>, Option<DiscordWiring>), DynError> {
    let Some(discord_cfg) = cfg.channels.discord.as_ref() else {
        return Ok((None, None));
    };
    let store_path = discord::allowlist_path(&cfg.data_dir());
    let initial_allowed = discord::store::load(&store_path).await.map_err(|err| {
        format!(
            "failed to load discord allowlist from {}: {err}",
            store_path.display()
        )
    })?;
    let (channel, control) = DiscordChannel::build(
        ChannelID::new(DISCORD_CHANNEL),
        discord_cfg,
        initial_allowed,
        store_path,
    );
    if discord_cfg.enabled
        && let Err(err) = control.enable().await
    {
        return Err(format!(
            "discord auto-enable failed (set channels.discord.enabled = false to skip): {err}"
        )
        .into());
    }
    Ok((Some(channel), Some(DiscordWiring { control })))
}

/// Resolve the `WeChat` channel + control pair from config.
async fn build_wechat(
    cfg: &AppConfig,
) -> Result<(Option<WechatChannel>, Option<WechatWiring>), DynError> {
    let Some(wechat_cfg) = cfg.channels.wechat.as_ref() else {
        return Ok((None, None));
    };
    let store_path = wechat::allowlist_path(&cfg.data_dir());
    let initial_allowed = wechat::store::load_allowlist(&store_path)
        .await
        .map_err(|err| {
            format!(
                "failed to load wechat allowlist from {}: {err}",
                store_path.display()
            )
        })?;
    let (channel, control) = WechatChannel::build(
        ChannelID::new(WECHAT_CHANNEL),
        wechat_cfg,
        initial_allowed,
        store_path,
        cfg.data_dir(),
    );
    if wechat_cfg.enabled
        && let Err(err) = control.enable().await
    {
        return Err(format!(
            "wechat auto-enable failed (set channels.wechat.enabled = false to skip): {err}"
        )
        .into());
    }
    Ok((Some(channel), Some(WechatWiring { control })))
}
