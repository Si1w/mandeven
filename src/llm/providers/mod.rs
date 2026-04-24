//! Per-provider [`BaseLLMClient`] implementations.
//!
//! Each provider file is self-contained: its base URL, authentication,
//! wire-format logic, and [`BaseLLMClient`] implementation all live in
//! the same file. New providers are added by creating a sibling file,
//! extending [`client_for`], and appending the new name to
//! [`REGISTERED`].

pub mod mistral;

use std::sync::Arc;

use super::client::BaseLLMClient;

/// Names of every provider baked into this build, in declaration order.
///
/// Kept in lock-step with the match arms in [`client_for`]: adding a
/// provider means adding an arm **and** an entry here. Downstream code
/// (notably the interactive bootstrap in `config::bootstrap`) reads
/// this list to enumerate provider choices without hard-coding a
/// specific name.
pub const REGISTERED: &[&str] = &["mistral"];

/// Return a shared client instance for the given provider name.
///
/// Returns `None` when the name does not match any provider in
/// [`REGISTERED`].
#[must_use]
pub fn client_for(name: &str) -> Option<Arc<dyn BaseLLMClient>> {
    match name {
        "mistral" => Some(Arc::new(mistral::Mistral::new())),
        _ => None,
    }
}
