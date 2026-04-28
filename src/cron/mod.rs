//! Cron — agent-internal scheduler that fires predefined prompts into
//! the agent on a recurring schedule.
//!
//! Cron is **not** a channel: it has no external source, owns no
//! [`crate::bus::ChannelID`], and produces no
//! [`crate::bus::InboundMessage`]. It lives next to the agent loop and
//! pushes ticks through a dedicated mpsc, mirroring
//! [`crate::heartbeat`]'s engine pattern with a richer schedule grammar
//! (`at` / `every` / `cron`) and N persisted jobs instead of one
//! markdown checklist.
//!
//! ## Persistence
//!
//! Unlike heartbeat (which has no state — `next_tick = now + interval`),
//! cron persists job definitions and runtime state to
//! `<data_dir>/cron/jobs.json`. Both `at`-job completion bookkeeping
//! and runtime-added jobs need a place to land that survives restarts;
//! `mandeven.toml` is read-only at runtime and not the right home for
//! agent-mutable data. Mirrors openclaw / nanobot / claw0.

pub mod engine;
pub mod error;
pub mod schedule;
pub mod store;
pub mod types;

pub use engine::{AUTO_DISABLE_AFTER, CronEngine};
pub use error::{Error, Result};
pub use schedule::{Schedule, ScheduleError};
pub use store::{Store, StoreFile};
pub use types::{
    CronJob, CronJobState, CronJobUpdate, CronJobUpdateOutcome, CronStatus, CronTick, RunStatus,
};

use serde::{Deserialize, Serialize};

/// Subdirectory under [`crate::config::AppConfig::data_dir`] holding
/// `jobs.json`. Same naming convention as `sessions/` — kept aside
/// from `mandeven.toml` because cron data is mutated at runtime.
pub const CRON_SUBDIR: &str = "cron";

/// Filename inside [`CRON_SUBDIR`] holding job definitions and runtime
/// state. Single-file layout (vs openclaw's split jobs.json /
/// jobs-state.json) keeps the MVP simple — split if/when concurrent
/// editors become a real concern.
pub const CRON_STORE_FILENAME: &str = "jobs.json";

/// User-tunable knobs for the cron engine.
///
/// Intentionally minimal: enable / disable is the only knob today.
/// Per-job knobs live in `jobs.json` so they can be edited at runtime
/// without touching `mandeven.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CronConfig {
    /// When `false`, the agent constructs without spawning the cron
    /// tick task. Default `true` so cron works out of the box once
    /// `jobs.json` exists.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
        }
    }
}

fn default_enabled() -> bool {
    true
}
