//! Channels — adapters between external sources (terminal, future IM
//! platforms) and the internal [`crate::bus`].
//!
//! Each concrete channel implements [`Channel`]:
//!
//! - [`Channel::start`] is a long-running listener that reads its
//!   source and publishes [`crate::bus::InboundMessage`]s.
//! - [`Channel::send`] delivers one
//!   [`crate::bus::OutboundMessage`] back to the source.
//!
//! [`Manager`] owns the single outbound receiver, spawns every
//! registered channel's `start` task, and routes each outbound
//! message to the channel whose [`ChannelID`] matches.
//!
//! Layout convention:
//!
//! - **Terminal / local adapters** (`tui/`) live at the crate root and
//!   implement [`Channel`] directly.
//! - **External / network adapters** (future: `discord/`, `slack/`,
//!   `telegram/`, …) live as subdirectories of this module.
//
// Session routing today is owned by `gateway`: channels emit
// identity-only `InboundMessage`s, and the gateway attaches the
// concrete `SessionID` before dispatching to the agent. The active
// session key includes channel/account/guild/peer identity; channel
// implementations should keep publishing identity and leave session
// choice out of individual adapters.

pub mod common;
pub mod discord;
pub mod error;
pub mod manager;
pub mod wechat;

pub use error::{Error, Result};
pub use manager::Manager;

use async_trait::async_trait;

use crate::bus::{ChannelID, InboundSender, OutboundMessage};

/// Contract for every input/output adapter registered with
/// [`Manager`].
#[async_trait]
pub trait Channel: Send + Sync {
    /// Stable identifier used to route outbound messages to this
    /// channel.
    fn id(&self) -> &ChannelID;

    /// Read the channel's source and publish inbound messages until
    /// the source closes. Returns `Ok(())` on clean shutdown.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on source / terminal failures and
    /// [`Error::Bus`] when the inbound bus has been closed.
    async fn start(&self, inbound: InboundSender) -> Result<()>;

    /// Deliver one outbound message to the source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on rendering or delivery failure. The
    /// manager logs the error and continues; it does not remove the
    /// channel from its routing table, so transient failures do not
    /// disable the channel permanently.
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}
