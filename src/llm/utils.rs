//! Internal helpers shared across provider implementations.
//!
//! Items here are crate-private and exist to keep provider files
//! focused on HTTP and wire-format logic rather than small mapping
//! utilities that would otherwise repeat across providers.

use super::types::FinishReason;

/// Parse a provider-returned finish-reason string into [`FinishReason`].
///
/// Unknown values are preserved via [`FinishReason::Other`] so they
/// remain observable in logs and telemetry instead of being silently
/// dropped.
#[must_use]
pub(crate) fn parse_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        other => FinishReason::Other(other.to_string()),
    }
}
