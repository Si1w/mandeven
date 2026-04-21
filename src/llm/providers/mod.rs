//! Per-provider [`BaseLLMClient`] implementations.
//!
//! Each provider file is self-contained: its base URL, authentication,
//! wire-format logic, and [`BaseLLMClient`] implementation all live in
//! the same file. New providers are added by creating a sibling file
//! and extending [`client_for`].

pub mod mistral;

use std::sync::Arc;

use super::client::BaseLLMClient;

/// Return a shared client instance for the given provider name.
///
/// Returns `None` when the name does not match any registered provider.
#[must_use]
pub fn client_for(name: &str) -> Option<Arc<dyn BaseLLMClient>> {
    match name {
        "mistral" => Some(Arc::new(mistral::Mistral::new())),
        _ => None,
    }
}
