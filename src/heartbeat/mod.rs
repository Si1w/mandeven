//! Heartbeat — agent-internal periodic timer that fires "wake up"
//! signals so the agent can react to time passing when no user input
//! arrives.
//!
//! Heartbeat is **not** a channel: it has no external source, owns no
//! [`crate::bus::ChannelID`], and produces no
//! [`crate::bus::InboundMessage`]. It lives next to the agent loop and
//! pushes ticks through a dedicated mpsc, which the agent races
//! against its inbound dispatch queue with `tokio::select!`.
//!
//! Each [`crate::agent::Agent`] instance owns its own
//! [`HeartbeatEngine`]. Multi-agent installations hold N engines (no
//! shared scheduler), in contrast to openclaw's single runner
//! managing a Map-of-N — see [`HeartbeatConfig`] for why we picked
//! the per-agent variant.

pub mod engine;

pub use engine::{HeartbeatEngine, HeartbeatStatus, HeartbeatTick};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default tick period in seconds (30 minutes), aligned with both
/// nanobot's `HeartbeatConfig.interval_s` default and openclaw's
/// `agents.defaults.heartbeat.every: "30m"` default.
const DEFAULT_INTERVAL_SECS: u64 = 1800;

/// Default prompt-file name resolved relative to the workspace root,
/// matching nanobot / openclaw's `HEARTBEAT.md` convention.
const DEFAULT_PROMPT_FILE: &str = "HEARTBEAT.md";

/// User-tunable knobs for the heartbeat engine.
///
/// Field names track openclaw's `agents.defaults.heartbeat` schema so
/// we can grow into per-agent overrides without renaming. See
/// `agent-examples/openclaw/docs/gateway/heartbeat.md` for the
/// reference shape.
///
/// ## Multi-agent path
///
/// We picked "one engine per [`crate::agent::Agent`] instance" rather
/// than openclaw's "single runner managing a Map of per-agent state".
/// When multi-agent lands ([`crate::session`] `TODO(multi-agent)`),
/// each `Agent` will own its own [`HeartbeatEngine`] constructed
/// from its own `HeartbeatConfig`. The corresponding config evolution
/// is `[agent.heartbeat]` becoming `[agent.list.<name>.heartbeat]`
/// (deep-merged on top of an `[agent.defaults.heartbeat]` block) —
/// same shape as openclaw, just realized as N engine instances
/// instead of one Map-of-N runner.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HeartbeatConfig {
    /// When `false`, the agent constructs without spawning a tick
    /// task. Default `true` so heartbeat works out of the box.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Tick period in seconds. Mutable at runtime via
    /// [`HeartbeatEngine::set_interval`] (`/heartbeat interval
    /// <secs>`); runtime changes are not persisted back to
    /// `mandeven.toml`.
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,

    /// Path of the prompt source, resolved against the workspace root
    /// ([`crate::config::AppConfig::data_dir`]). The file's contents
    /// become the phase-2 user message. A missing or effectively-empty
    /// file causes the tick to be skipped (matches openclaw's
    /// `reason=empty-heartbeat-file` behavior).
    #[serde(default = "default_prompt_file")]
    pub prompt_file: PathBuf,
    //
    // TODO(model-override): phase-1 (decide tool) and phase-2 (full
    // iteration) currently both reuse the agent's main `[llm.default]`
    // profile. When per-tick token cost becomes a concern, add an
    // optional `model: Option<String>` here so heartbeat runs can be
    // routed through a cheaper / faster model. Same `"provider/model"`
    // qualified-name format the rest of our config already uses.
    // Reference: openclaw's `agents.defaults.heartbeat.model`.
    //
    // TODO(target-routing): outbound for heartbeat is currently
    // hard-coded to the `tui` channel (the only channel registered
    // today). When more channels land (discord, telegram, ...),
    // introduce `target: "last" | "none" | "<channel-id>"` plus
    // optional `to` and `accountId`, mirroring openclaw's
    // `agents.defaults.heartbeat.{target,to,accountId}` and the
    // precedence rules in
    // `agent-examples/openclaw/docs/gateway/heartbeat.md`.
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            interval_secs: default_interval_secs(),
            prompt_file: default_prompt_file(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_interval_secs() -> u64 {
    DEFAULT_INTERVAL_SECS
}

fn default_prompt_file() -> PathBuf {
    PathBuf::from(DEFAULT_PROMPT_FILE)
}
