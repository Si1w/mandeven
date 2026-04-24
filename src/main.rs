//! Mandeven — terminal agent bootstrap.
//!
//! Wires the domain modules together:
//! [`bus`](mandeven::bus) for in-process messaging,
//! [`agent`](mandeven::agent) for the iteration loop,
//! [`channels`](mandeven::channels) for the channel registry + router,
//! and [`cli`](mandeven::cli) as the currently-registered TUI channel.
//! Requires `MISTRAL_API_KEY` in the environment and `./mandeven.toml`
//! in the working directory.

use std::sync::Arc;

use mandeven::agent::Agent;
use mandeven::bus::{Bus, ChannelID, SessionID};
use mandeven::channels::Manager;
use mandeven::cli::CliChannel;
use mandeven::config::AppConfig;
use mandeven::session;
use mandeven::tools;

/// Directory under the config's `data_dir` where session files live.
const SESSION_SUBDIR: &str = "sessions";

/// Identifier for the built-in TUI channel.
const TUI_CHANNEL: &str = "tui";

/// Boxed error alias used at the `main` boundary.
type DynError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), DynError> {
    let cfg = AppConfig::load()?;
    let sessions = Arc::new(session::Manager::new(cfg.data_dir().join(SESSION_SUBDIR)).await?);

    let (bus, inbound_rx, outbound_rx) = Bus::new();
    let inbound_tx = bus.inbound_sender();
    let outbound_tx = bus.outbound_sender();
    drop(bus);

    let mut tool_registry = tools::Registry::new();
    tools::register_builtins(&mut tool_registry);
    let agent = Agent::new(&cfg, sessions, tool_registry, inbound_rx, outbound_tx)?;

    let mut manager = Manager::new(outbound_rx);
    manager.register(Arc::new(CliChannel::new(
        ChannelID::new(TUI_CHANNEL),
        SessionID::new(),
    )));

    let agent_handle = tokio::spawn(agent.run());
    manager.run(inbound_tx).await?;
    agent_handle.await??;
    Ok(())
}
