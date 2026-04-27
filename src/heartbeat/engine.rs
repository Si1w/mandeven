//! The [`HeartbeatEngine`] — periodic tick driver bolted onto a single
//! [`crate::agent::Agent`] instance.
//!
//! Each `Agent` constructs its own engine from its own
//! [`HeartbeatConfig`]; engines do not coordinate with each other.
//! Multi-agent installations will simply hold N engines.
//!
//! Tick path:
//!
//! 1. Background task wakes on `interval_secs` (or earlier when
//!    [`HeartbeatEngine::trigger`] forces a wake).
//! 2. If `paused`, the task waits for the next wake without producing
//!    a tick.
//! 3. Otherwise it reads the prompt file (resolved against the agent
//!    workspace) and pushes a [`HeartbeatTick`] to the agent loop. A
//!    missing or effectively-empty file causes the tick to be skipped
//!    (matches openclaw's `reason=empty-heartbeat-file`).
//!
//! The agent receives ticks through the matching
//! `mpsc::Receiver<HeartbeatTick>` returned from [`HeartbeatEngine::new`]
//! and races them against its inbound dispatch queue with
//! `tokio::select!`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::fs;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::{HEARTBEAT_FILENAME, HeartbeatConfig};

/// Capacity of the engine → agent tick queue. Ticks are rare events
/// (default 30-minute period); a tiny buffer is plenty and any
/// further build-up is a sign the agent loop is hung.
const TICK_QUEUE_CAPACITY: usize = 4;

/// Smallest runtime interval accepted by the engine. Config loading
/// rejects zero, but direct callers of the engine API still get a
/// defensive clamp instead of a `sleep(Duration::ZERO)` loop.
const MIN_INTERVAL_SECS: u64 = 1;

/// One tick delivered from the engine to the agent loop.
#[derive(Clone, Debug)]
pub struct HeartbeatTick {
    /// Verbatim contents of the resolved prompt file. The agent uses
    /// this as the phase-2 user message.
    pub content: String,
    /// Wall-clock timestamp at which the tick fired.
    pub at: DateTime<Utc>,
}

/// Snapshot of the engine's runtime state — what `/heartbeat` (the
/// status command) renders.
#[derive(Clone, Debug)]
pub struct HeartbeatStatus {
    /// `enabled` value the engine was constructed with.
    pub enabled: bool,
    /// `true` when the user has paused the engine via
    /// [`HeartbeatEngine::pause`].
    pub paused: bool,
    /// Current tick period.
    pub interval_secs: u64,
    /// Last time the engine emitted (or attempted) a tick. `None`
    /// before the first wake.
    pub last_tick_at: Option<DateTime<Utc>>,
    /// Approximate seconds until the next wake. `None` when the
    /// engine is paused or has not started.
    pub next_tick_in_secs: Option<u64>,
}

/// Heartbeat engine.
///
/// Public API is intentionally thin: construction returns the engine
/// **and** the matching tick receiver, the agent stores both, and
/// commands flip simple flags. The engine is `Clone`-free; share via
/// `Arc<HeartbeatEngine>` when more than one place (agent loop +
/// command handlers) needs to reach the controls.
pub struct HeartbeatEngine {
    state: Arc<Mutex<EngineState>>,
    /// Wakes the tick task early — used by [`HeartbeatEngine::trigger`]
    /// and configuration changes (`set_interval`, `resume`).
    wake: Arc<Notify>,
    workspace: PathBuf,
    tick_tx: mpsc::Sender<HeartbeatTick>,
}

/// Mutable state shared between the engine handle and the tick task.
struct EngineState {
    interval: Duration,
    paused: bool,
    enabled: bool,
    last_tick_at: Option<DateTime<Utc>>,
    /// Monotonic anchor used for the "next tick in X seconds" status
    /// readout. Updated whenever the tick task starts a new wait.
    last_wake_started_at: Option<Instant>,
    handle: Option<JoinHandle<()>>,
}

impl HeartbeatEngine {
    /// Construct an engine + paired tick receiver.
    ///
    /// The engine is created in a stopped state — call
    /// [`Self::start`] to spawn the background tick task. The
    /// receiver half goes to the agent loop, which races it against
    /// its inbound dispatch queue.
    #[must_use]
    pub fn new(
        config: &HeartbeatConfig,
        workspace: &Path,
    ) -> (Self, mpsc::Receiver<HeartbeatTick>) {
        let (tick_tx, tick_rx) = mpsc::channel(TICK_QUEUE_CAPACITY);
        let state = EngineState {
            interval: interval_duration(config.interval_secs),
            paused: false,
            enabled: config.enabled,
            last_tick_at: None,
            last_wake_started_at: None,
            handle: None,
        };
        let engine = Self {
            state: Arc::new(Mutex::new(state)),
            wake: Arc::new(Notify::new()),
            // Resolve the well-known prompt file once at
            // construction time, against the agent's data directory.
            workspace: workspace.join(HEARTBEAT_FILENAME),
            tick_tx,
        };
        (engine, tick_rx)
    }

    /// Spawn the background tick task. No-op when the engine was
    /// constructed with `enabled = false` or when a task is already
    /// running.
    ///
    /// # Panics
    ///
    /// Panics if the engine state mutex was poisoned.
    pub fn start(self: &Arc<Self>) {
        let mut state = self.state.lock().expect("heartbeat state poisoned");
        if !state.enabled || state.handle.is_some() {
            return;
        }
        let me = Arc::clone(self);
        let handle = tokio::spawn(run_tick_loop(me));
        state.handle = Some(handle);
    }

    /// Stop the tick task and wait for it to drain.
    ///
    /// # Panics
    ///
    /// Panics if the engine state mutex was poisoned by a prior
    /// crash on a tick task — irrecoverable, so the panic is the
    /// honest answer.
    pub async fn shutdown(&self) {
        let handle = {
            let mut state = self.state.lock().expect("heartbeat state poisoned");
            state.handle.take()
        };
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
        }
    }

    /// Pause tick emission. The background task keeps running so the
    /// next [`Self::resume`] takes effect immediately.
    ///
    /// # Panics
    ///
    /// Panics if the engine state mutex was poisoned.
    pub fn pause(&self) {
        let mut state = self.state.lock().expect("heartbeat state poisoned");
        state.paused = true;
    }

    /// Resume tick emission and wake the task so the next tick fires
    /// promptly rather than after the remaining backoff.
    ///
    /// # Panics
    ///
    /// Panics if the engine state mutex was poisoned.
    pub fn resume(&self) {
        {
            let mut state = self.state.lock().expect("heartbeat state poisoned");
            state.paused = false;
        }
        self.wake.notify_one();
    }

    /// Replace the tick period. Wakes the task so the new interval
    /// takes effect immediately rather than after the in-flight wait.
    /// Values below one second are clamped to one second.
    ///
    /// # Panics
    ///
    /// Panics if the engine state mutex was poisoned.
    pub fn set_interval(&self, secs: u64) {
        {
            let mut state = self.state.lock().expect("heartbeat state poisoned");
            state.interval = interval_duration(secs);
        }
        self.wake.notify_one();
    }

    /// Force a tick on the next scheduler tick, bypassing the current
    /// interval. Honors `paused` — a paused engine ignores triggers.
    pub fn trigger(&self) {
        self.wake.notify_one();
    }

    /// Snapshot the engine's runtime state.
    ///
    /// # Panics
    ///
    /// Panics if the engine state mutex was poisoned.
    #[must_use]
    pub fn status(&self) -> HeartbeatStatus {
        let state = self.state.lock().expect("heartbeat state poisoned");
        let next_tick_in_secs = if state.paused {
            None
        } else {
            state.last_wake_started_at.map(|started| {
                let elapsed = started.elapsed();
                state.interval.saturating_sub(elapsed).as_secs()
            })
        };
        HeartbeatStatus {
            enabled: state.enabled,
            paused: state.paused,
            interval_secs: state.interval.as_secs(),
            last_tick_at: state.last_tick_at,
            next_tick_in_secs,
        }
    }
}

/// Body of the background tick task spawned by [`HeartbeatEngine::start`].
///
/// Each iteration:
///
/// 1. Snapshots the current interval (it may have been swapped by
///    [`HeartbeatEngine::set_interval`]) and stamps `last_wake_started_at`.
/// 2. Sleeps until the interval elapses or [`HeartbeatEngine::trigger`]
///    /[`HeartbeatEngine::resume`] / [`HeartbeatEngine::set_interval`]
///    notify early.
/// 3. Re-checks `paused` (the wake might be `resume` itself, but it
///    might also be `set_interval` while still paused) — paused ⇒ skip.
/// 4. Reads the prompt file. Missing or effectively empty ⇒ skip
///    (matches openclaw's `reason=empty-heartbeat-file`).
/// 5. Pushes a [`HeartbeatTick`]; the agent's `select!` picks it up.
///    A closed receiver shuts the task down — happens during
///    [`HeartbeatEngine::shutdown`] or normal process exit.
async fn run_tick_loop(engine: Arc<HeartbeatEngine>) {
    loop {
        let interval = {
            let mut state = engine.state.lock().expect("heartbeat state poisoned");
            state.last_wake_started_at = Some(Instant::now());
            state.interval
        };

        tokio::select! {
            () = sleep(interval) => {}
            () = engine.wake.notified() => {}
        }

        if engine
            .state
            .lock()
            .expect("heartbeat state poisoned")
            .paused
        {
            continue;
        }

        let content = match fs::read_to_string(&engine.workspace).await {
            Ok(text) if !is_effectively_empty(&text) => text,
            _ => continue,
        };

        let now = Utc::now();
        {
            let mut state = engine.state.lock().expect("heartbeat state poisoned");
            state.last_tick_at = Some(now);
        }

        let tick = HeartbeatTick { content, at: now };
        if engine.tick_tx.send(tick).await.is_err() {
            return;
        }
    }
}

/// Heuristic for "the prompt file has nothing actionable".
///
/// Mirrors openclaw's empty-heartbeat-file rule: lines that are blank
/// or pure markdown headings (`#`, `##`, …) are skipped. Anything else
/// counts as content.
fn is_effectively_empty(content: &str) -> bool {
    content.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty() || trimmed.starts_with('#')
    })
}

fn interval_duration(secs: u64) -> Duration {
    Duration::from_secs(secs.max(MIN_INTERVAL_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine(enabled: bool, interval_secs: u64) -> Arc<HeartbeatEngine> {
        let cfg = HeartbeatConfig {
            enabled,
            interval_secs,
        };
        let (engine, _rx) = HeartbeatEngine::new(&cfg, Path::new("/tmp"));
        Arc::new(engine)
    }

    #[test]
    fn pause_then_resume_round_trips() {
        let engine = make_engine(true, 30);
        assert!(!engine.status().paused);
        engine.pause();
        assert!(engine.status().paused);
        engine.resume();
        assert!(!engine.status().paused);
    }

    #[test]
    fn set_interval_is_visible_in_status() {
        let engine = make_engine(true, 30);
        engine.set_interval(120);
        assert_eq!(engine.status().interval_secs, 120);
    }

    #[test]
    fn zero_interval_is_clamped_to_one_second() {
        let engine = make_engine(true, 0);
        assert_eq!(engine.status().interval_secs, 1);

        engine.set_interval(0);
        assert_eq!(engine.status().interval_secs, 1);
    }

    #[test]
    fn paused_engine_reports_no_next_tick() {
        let engine = make_engine(true, 30);
        engine.pause();
        assert_eq!(engine.status().next_tick_in_secs, None);
    }

    #[test]
    fn effectively_empty_recognizes_blank_and_heading_only() {
        assert!(is_effectively_empty(""));
        assert!(is_effectively_empty("   \n\n  "));
        assert!(is_effectively_empty("# Heartbeat\n\n## Section"));
        assert!(!is_effectively_empty("- check inbox"));
        assert!(!is_effectively_empty("# heading\nplus body"));
    }
}
