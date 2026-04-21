//! The [`BaseLLMClient`] trait.

use async_trait::async_trait;

use super::error::Result;
use super::types::{Request, Response, ResponseStream};

/// A self-contained client for one LLM provider.
///
/// Each provider owns its full HTTP, authentication, and wire-format
/// logic; the trait only prescribes the user-facing entry points.
#[async_trait]
pub trait BaseLLMClient: Send + Sync {
    /// Registered provider name (for example `"mistral"`). Must match
    /// the key used by [`super::providers::client_for`].
    fn name(&self) -> &'static str;

    /// Name of the environment variable holding this provider's API key.
    fn api_key_env(&self) -> &'static str;

    /// Issue a completion and block until the full response arrives.
    ///
    /// # Errors
    ///
    /// Returns [`super::Error`] on transport failure, non-success
    /// status, malformed response, or timeout.
    async fn complete(&self, req: Request) -> Result<Response>;

    /// Issue a completion and receive chunks as they arrive.
    ///
    /// # Errors
    ///
    /// Returns [`super::Error`] synchronously on request setup failures;
    /// per-chunk errors are surfaced through the returned stream.
    async fn stream(&self, req: Request) -> Result<ResponseStream>;
}
