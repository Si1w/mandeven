//! Errors surfaced by the `bus` module.

use thiserror::Error;

/// Errors that can occur while publishing on the bus.
///
/// This is intentionally minimal — publication only fails when the
/// downstream consumer's receiver has been dropped, which indicates
/// the agent loop or channel dispatcher is no longer running. Callers
/// usually suppress with `.ok()` and let the top-level daemon detect
/// the shutdown.
#[derive(Debug, Error)]
pub enum Error {
    /// The target receiver has been dropped.
    #[error("bus channel closed")]
    Closed,
}

/// Result alias for the `bus` module.
pub type Result<T> = std::result::Result<T, Error>;
