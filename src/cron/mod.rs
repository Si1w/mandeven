//! Cron — agent-internal scheduler that fires predefined prompts into
//! the agent on a recurring schedule.
//!
//! Cron is **not** a channel: it has no external source, owns no
//! [`crate::bus::ChannelID`], and produces no
//! [`crate::bus::InboundMessage`]. It lives next to the agent loop and
//! pushes ticks through a dedicated mpsc, mirroring
//! [`crate::heartbeat`]'s engine pattern with a richer schedule grammar
//! (cron expression / one-shot / fixed interval). Not yet wired into
//! `lib.rs`.
