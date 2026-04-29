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
use std::sync::Arc;

use tokio::sync::Mutex;

use mandeven::agent::{Agent, CronWiring, DiscordWiring, HeartbeatWiring, WechatWiring};
use mandeven::bus::{Bus, ChannelID};
use mandeven::channels::Manager;
use mandeven::channels::discord::{self, DiscordChannel};
use mandeven::channels::wechat::{self, WechatChannel};
use mandeven::cli::CliChannel;
use mandeven::config::{self, AppConfig};
use mandeven::cron::CronEngine;
use mandeven::gateway::{Gateway, dispatch_channel};
use mandeven::heartbeat::HeartbeatEngine;
use mandeven::hook::HookEngine;
use mandeven::memory;
use mandeven::prompt::PromptEngine;
use mandeven::security::SandboxPolicy;
use mandeven::session;
use mandeven::skill::{self, SkillIndex};
use mandeven::task;
use mandeven::tools;
use mandeven::utils::workspace;

/// Identifier for the built-in TUI channel.
const TUI_CHANNEL: &str = "tui";

/// Identifier for the Discord channel adapter.
const DISCORD_CHANNEL: &str = "discord";

/// Identifier for the WeChat channel adapter.
const WECHAT_CHANNEL: &str = "wechat";

/// Boxed error alias used at the `main` boundary.
type DynError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let cfg = AppConfig::bootstrap()?;

    // Capture the launch CWD once. The canonical form anchors the
    // workspace boundary every tool reads via `workspace::root()`; the
    // raw form drives the per-project session bucket.
    let cwd = std::env::current_dir()?;
    let canonical_cwd = std::fs::canonicalize(&cwd)?;
    workspace::init(canonical_cwd);

    // Install the sandbox tier before any tool is registered. Tools read
    // it via `SandboxPolicy::current()` on each invocation; missing
    // `[sandbox]` block in the TOML keeps the default `WorkspaceWrite`.
    SandboxPolicy::init(cfg.sandbox.policy);

    // Sessions are scoped per-project: same `~/.mandeven/projects/`
    // bucket shape as Claude Code's `~/.claude/projects/<sanitized-cwd>/`
    // — see agent-examples/claude-code-analysis/src/utils/sessionStoragePortable.ts.
    let sessions = Arc::new(session::Manager::new(config::project_bucket(&cwd)).await?);

    // Skill index reads ~/.mandeven/skills/<name>/SKILL.md once at
    // boot. Disabled => empty index, no SkillTool registration, no
    // skills_index section in the prompt.
    let skill_index = Arc::new(if cfg.agent.skill.enabled {
        skill::load(&cfg.data_dir().join(skill::SKILLS_SUBDIR))?
    } else {
        SkillIndex::new()
    });

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
    // agent reads it (heartbeat tick path) so heartbeat ticks land in
    // the user's main session rather than spinning up an isolated one.
    let active_sessions = Arc::new(Mutex::new(HashMap::new()));

    let heartbeat_wiring = if cfg.agent.heartbeat.enabled {
        let data_dir = cfg.data_dir();
        let (engine, rx) = HeartbeatEngine::new(&cfg.agent.heartbeat, &data_dir);
        let engine = Arc::new(engine);
        engine.start();
        Some(HeartbeatWiring { engine, rx })
    } else {
        None
    };

    let cron_wiring = if cfg.agent.cron.enabled {
        let data_dir = cfg.data_dir();
        let (engine, rx) = CronEngine::new(&cfg.agent.cron, &data_dir).await?;
        let engine = Arc::new(engine);
        engine.start().await;
        Some(CronWiring { engine, rx })
    } else {
        None
    };

    let mut tool_registry = tools::Registry::new();
    tools::register_builtins(&mut tool_registry);
    let task_manager = Arc::new(task::Manager::new(&config::project_bucket(&cwd)));
    tools::task::register(&mut tool_registry, task_manager);
    let memory_manager = Arc::new(memory::Manager::new(
        &cfg.data_dir(),
        &config::project_bucket(&cwd),
    ));
    if cfg.agent.memory.enabled {
        tools::memory::register(&mut tool_registry, memory_manager.clone());
    }
    if let Some(wiring) = cron_wiring.as_ref() {
        tools::cron::register(&mut tool_registry, wiring.engine.clone());
    }
    if !skill_index.is_empty() {
        tool_registry.register(Arc::new(tools::skill::SkillTool::new(skill_index.clone())));
    }

    let (discord_channel, discord_wiring) = build_discord(&cfg).await?;
    let discord_active_rx = discord_wiring
        .as_ref()
        .map(|w| w.control.subscribe_active());
    let (wechat_channel, wechat_wiring) = build_wechat(&cfg).await?;
    let wechat_active_rx = wechat_wiring.as_ref().map(|w| w.control.subscribe_active());

    let agent = Agent::new(
        &cfg,
        sessions.clone(),
        tool_registry,
        dispatch_rx,
        outbound_tx.clone(),
        active_sessions.clone(),
        heartbeat_wiring,
        cron_wiring,
        memory_manager,
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

/// Resolve the WeChat channel + control pair from config.
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
