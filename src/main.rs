//! Mandeven — terminal agent bootstrap.
//!
//! Wires the domain modules together:
//! [`bus`](mandeven::bus) for in-process messaging,
//! [`gateway`](mandeven::gateway) for session routing,
//! [`agent`](mandeven::agent) for the iteration loop,
//! [`channels`](mandeven::channels) for the channel registry + router,
//! and [`cli`](mandeven::cli) as the currently-registered TUI channel.
//! Requires the configured provider's API key in the environment and
//! `~/.mandeven/mandeven.toml` (or the path under `$MANDEVEN_HOME`).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use mandeven::agent::{Agent, CronWiring, HeartbeatWiring};
use mandeven::bus::{Bus, ChannelID};
use mandeven::channels::Manager;
use mandeven::cli::CliChannel;
use mandeven::config::{self, AppConfig};
use mandeven::cron::CronEngine;
use mandeven::gateway::{Gateway, dispatch_channel};
use mandeven::heartbeat::HeartbeatEngine;
use mandeven::hook::HookEngine;
use mandeven::prompt::PromptEngine;
use mandeven::security::SandboxPolicy;
use mandeven::session;
use mandeven::skill::{self, SkillIndex};
use mandeven::tools;

/// Identifier for the built-in TUI channel.
const TUI_CHANNEL: &str = "tui";

/// Boxed error alias used at the `main` boundary.
type DynError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let cfg = AppConfig::bootstrap()?;

    // Install the sandbox tier before any tool is registered. Tools read
    // it via `SandboxPolicy::current()` on each invocation; missing
    // `[sandbox]` block in the TOML keeps the default `WorkspaceWrite`.
    SandboxPolicy::init(cfg.sandbox.policy);

    // Sessions are scoped per-project: capture the launch cwd once and
    // sanitize it into a bucket name under `~/.mandeven/projects/`.
    // Same shape as Claude Code's `~/.claude/projects/<sanitized-cwd>/`
    // — see agent-examples/claude-code-analysis/src/utils/sessionStoragePortable.ts.
    let cwd = std::env::current_dir()?;
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

    let mut tool_registry = tools::Registry::new();
    tools::register_builtins(&mut tool_registry);
    if !skill_index.is_empty() {
        tool_registry.register(Arc::new(tools::skill::SkillTool::new(skill_index.clone())));
    }

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

    let agent = Agent::new(
        &cfg,
        sessions.clone(),
        tool_registry,
        dispatch_rx,
        outbound_tx.clone(),
        active_sessions.clone(),
        heartbeat_wiring,
        cron_wiring,
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
    )));

    let agent_handle = tokio::spawn(agent.run());
    let gateway_handle = tokio::spawn(gateway.run());
    manager.run(inbound_tx).await?;
    agent_handle.await??;
    gateway_handle.await??;
    Ok(())
}
