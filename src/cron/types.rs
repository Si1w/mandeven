//! Cron job records and the wire-shape for status / tick messages.
//!
//! Field-naming follows the same camelCase-on-disk convention nanobot
//! and openclaw use, mapped to Rust `snake_case` via `serde(rename)`.
//! That keeps the on-disk JSON readable from any of the reference
//! projects' tooling without hand-editing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Schedule;

/// On-disk store version. Bump only when the JSON shape stops being
/// backward-compatible — adding new optional fields does not require
/// a bump because `serde(default)` handles missing keys.
pub const STORE_VERSION: u32 = 1;

/// Terminal status of one execution.
///
/// `Skipped` exists for "fire was skipped because target session was
/// missing" cases; present jobs end `Succeeded` or `Failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Succeeded,
    Failed,
    Skipped,
}

/// Mutable per-job runtime fields that change with each execution.
///
/// All timestamps are wall-clock UTC; relative durations
/// (`next_tick_in_secs` and friends) are computed from these on
/// demand, never stored.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CronJobState {
    /// Computed firing instant. `None` when the job is disabled or
    /// has no future fires (`at` job that already ran, `cron`
    /// expression with an exhausted iterator).
    #[serde(default, rename = "nextRunAt")]
    pub next_run_at: Option<DateTime<Utc>>,

    /// Wall-clock instant of the most recent execution. `None` until
    /// the first tick fires.
    #[serde(default, rename = "lastRunAt")]
    pub last_run_at: Option<DateTime<Utc>>,

    /// Outcome of the most recent execution.
    #[serde(default, rename = "lastStatus")]
    pub last_status: Option<RunStatus>,

    /// Error message captured on the most recent failed run. Cleared
    /// on the next successful one.
    #[serde(default, rename = "lastError")]
    pub last_error: Option<String>,

    /// Consecutive failure count. Resets to 0 on success; the engine
    /// auto-disables the job once it reaches
    /// [`super::engine::AUTO_DISABLE_AFTER`].
    #[serde(default, rename = "consecutiveErrors")]
    pub consecutive_errors: u32,
}

/// One scheduled cron job — definition plus runtime state.
///
/// `id` is a freshly-minted UUID v7 string so on-disk records are
/// chronologically sortable just like [`crate::bus::SessionID`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CronJob {
    /// UUID v7 string. Stable across edits — the engine matches by
    /// id, not by name.
    pub id: String,

    /// Human-readable label rendered in `/cron` output.
    pub name: String,

    /// `false` mutes the job — it stays in the store but never fires.
    /// The engine flips this to `false` automatically after consecutive
    /// failures or after a one-shot completes.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Schedule rule. Round-trips through
    /// [`super::schedule::Schedule`]'s serde shim.
    pub schedule: Schedule,

    /// Prompt string fed to the agent as the user message. Cron does
    /// not run a phase-1 decide step (unlike heartbeat), so this text
    /// flows directly into `Agent::iteration`.
    pub prompt: String,

    /// Mutable state. Defaults to all-`None` at creation; the engine
    /// fills [`CronJobState::next_run_at`] before persisting.
    #[serde(default)]
    pub state: CronJobState,

    /// One-shot bookkeeping: when `true` and `schedule` is `at`, the
    /// job is removed from the store after a successful run. When
    /// `false` (default), one-shots stay around with `enabled: false`
    /// so the user can inspect history.
    #[serde(default, rename = "deleteAfterRun")]
    pub delete_after_run: bool,

    /// Wall-clock creation timestamp.
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,

    /// Wall-clock timestamp of the most recent definition or state
    /// mutation. Differs from [`CronJobState::last_run_at`] which is
    /// strictly the last execution.
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

impl CronJob {
    /// Mint a fresh job at `now` with all-default state.
    ///
    /// The engine recomputes [`CronJobState::next_run_at`] before
    /// returning to the caller; this method intentionally leaves it
    /// `None` so unit tests don't depend on a clock.
    #[must_use]
    pub fn new(name: String, schedule: Schedule, prompt: String, now: DateTime<Utc>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            name,
            enabled: true,
            schedule,
            prompt,
            state: CronJobState::default(),
            delete_after_run: false,
            created_at: now,
            updated_at: now,
        }
    }
}

fn default_enabled() -> bool {
    true
}

/// One tick delivered from the engine to the agent loop. Carries
/// enough context that the agent can label the resulting iteration
/// in logs and route the outbound stream back to the right channel.
#[derive(Clone, Debug)]
pub struct CronTick {
    /// `id` of the firing job — unique key for telemetry.
    pub job_id: String,

    /// `name` for human-readable log lines.
    pub job_name: String,

    /// User-message text fed to the agent. Already validated
    /// non-empty when the job was added.
    pub prompt: String,

    /// Wall-clock instant the engine computed as the "fire time" —
    /// used for `last_run_at` bookkeeping and tick logging.
    pub at: DateTime<Utc>,
}

/// Snapshot of the engine's state — what `/cron` (no args) renders.
///
/// Exposes full [`CronJob`] records (rather than a UI-flattened
/// projection) because the command layer is the only consumer and
/// the duplication of a separate `JobStatus` type would buy nothing.
/// Cloning is acceptable — `status` is a low-frequency call.
#[derive(Clone, Debug)]
pub struct CronStatus {
    /// `enabled` value the engine was constructed with.
    pub enabled: bool,
    /// All registered jobs, in insertion order.
    pub jobs: Vec<CronJob>,
}
