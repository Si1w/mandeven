//! Mandeven — terminal agent bootstrap.
//!
//! Wires the domain modules together:
//! [`bus`](mandeven::bus) for in-process messaging,
//! [`gateway`](mandeven::gateway) for session routing,
//! [`agent`](mandeven::agent) for the iteration loop,
//! [`channels`](mandeven::channels) for the channel registry + router,
//! and [`cli`](mandeven::cli) as the currently-registered TUI channel.
//! Requires `MISTRAL_API_KEY` in the environment and `./mandeven.toml`
//! in the working directory.

use std::sync::Arc;

use mandeven::agent::Agent;
use mandeven::bus::{Bus, ChannelID};
use mandeven::channels::Manager;
use mandeven::cli::CliChannel;
use mandeven::config::AppConfig;
use mandeven::gateway::{Gateway, dispatch_channel};
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
    let cfg = AppConfig::bootstrap()?;
    let sessions = Arc::new(session::Manager::new(cfg.data_dir().join(SESSION_SUBDIR)).await?);

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
    let agent = Agent::new(
        &cfg,
        sessions.clone(),
        tool_registry,
        dispatch_rx,
        outbound_tx.clone(),
    )?;

    let gateway = Gateway::new(inbound_rx, dispatch_tx, outbound_tx, sessions.clone());

    let mut manager = Manager::new(outbound_rx);
    manager.register(Arc::new(CliChannel::new(
        ChannelID::new(TUI_CHANNEL),
        sessions,
    )));

    let agent_handle = tokio::spawn(agent.run());
    let gateway_handle = tokio::spawn(gateway.run());
    manager.run(inbound_tx).await?;
    agent_handle.await??;
    gateway_handle.await??;
    Ok(())
}
