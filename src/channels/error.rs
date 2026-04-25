//! Errors surfaced by the `channels` module.

use thiserror::Error;

use crate::bus;

/// Errors that can occur while a channel runs or while the
/// [`super::Manager`] routes outbound messages.
#[derive(Debug, Error)]
pub enum Error {
    /// Propagated from terminal / stdio or other I/O operations a
    /// channel performs while interacting with its source.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Propagated from publishing on the bus.
    #[error("bus error: {0}")]
    Bus(#[from] bus::Error),

    /// Propagated when a channel reads session history (for
    /// example rebuilding its transcript after a gateway-announced
    /// session switch).
    #[error("session error: {0}")]
    Session(#[from] crate::session::Error),
}

/// Result alias for the `channels` module.
pub type Result<T> = std::result::Result<T, Error>;
