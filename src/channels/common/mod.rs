//! Cross-channel helpers shared by IM adapters.
//!
//! These utilities are platform-agnostic by design — they operate on
//! plain text and identifiers, never on platform-specific message
//! handles. Each concrete adapter (Discord, future Slack/Telegram)
//! keeps its own state for "where to send" and reuses these modules
//! for chunking, throttled streaming, and inbound permission checks.

pub mod allowlist;
pub mod chunk;
pub mod stream_buf;

pub use allowlist::AllowList;
pub use chunk::split_message;
pub use stream_buf::{FinalizeResult, StreamAction, StreamBuf};
