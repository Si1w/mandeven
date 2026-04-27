//! The [`CronEngine`] — single-timer driver that walks N persisted
//! jobs, fires due ones into the agent loop, and tracks runtime state.
//!
//! The shape mirrors [`crate::heartbeat::HeartbeatEngine`] (background
//! task + `mpsc::Sender<Tick>` to the agent + `Notify` for early
//! wakes). The non-trivial differences are:
//!
//! - **N jobs, not one prompt.** Each tick carries the firing job's
//!   id / name / prompt so the agent can route correctly.
//! - **Persistent state.** Job definitions, last-run, last-status, and
//!   error counters live in `<data_dir>/cron/jobs.json`. Every
//!   mutation goes through [`Store::save`] under the engine's lock so
//!   on-disk state never lags behind in-memory state.
//! - **Strict-after firing.** The tick task's wake target is
//!   `min(next_run_at)` across enabled jobs; on wake we collect *all*
//!   due jobs (handles bursts when wall-clock jumps), advance their
//!   `next_run_at` via [`Schedule::next_after`], persist, then send.
//! - **Persist-then-send.** If the persist fails, we never send the
//!   tick — that prevents double-firing after a crash.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::error::{Error, Result};
use super::schedule::Schedule;
use super::store::{Store, StoreFile};
use super::types::{CronJob, CronStatus, CronTick, RunStatus, STORE_VERSION};
use super::{CRON_SUBDIR, CronConfig};

/// Capacity of the engine → agent tick queue. Cron ticks are rare
/// (sub-minute granularity at the floor); a tiny buffer is plenty
/// and any further build-up indicates a hung agent loop.
const TICK_QUEUE_CAPACITY: usize = 16;

/// Wake at least once per [`MAX_SLEEP`] even when no job is pending.
/// Recovers schedule drift after wall-clock jumps (suspend / resume,
/// NTP step), matching openclaw's `MAX_TIMER_DELAY_MS = 60_000`.
const MAX_SLEEP: Duration = Duration::from_mins(1);

/// Consecutive failure threshold beyond which the engine flips a
/// job's `enabled` to `false`. Matches claw0's choice and is in the
/// same ballpark as openclaw's per-job retry budget.
pub const AUTO_DISABLE_AFTER: u32 = 5;

/// Cron engine. Owns the persistent store and the tick task; the
/// agent holds it through `Arc<CronEngine>` so command handlers can
/// reach the same controls.
pub struct CronEngine {
    state: Arc<AsyncMutex<EngineState>>,
    /// Wakes the tick task early — used after every mutation so the
    /// loop re-reads the schedule (e.g. an added job has an earlier
    /// `next_run_at` than the previously computed wake target).
    wake: Arc<Notify>,
    store: Store,
    tick_tx: mpsc::Sender<CronTick>,
    /// Cached so [`status`] can render the configured value even
    /// after `enabled = false` keeps the loop dormant.
    enabled: bool,
}

/// Mutable state shared between the engine handle and the tick task.
struct EngineState {
    jobs: Vec<CronJob>,
    handle: Option<JoinHandle<()>>,
}

impl CronEngine {
    /// Construct an engine + paired tick receiver.
    ///
    /// Loads `<data_dir>/cron/jobs.json` (creating an empty store if
    /// the file is missing) and recomputes `next_run_at` for every
    /// enabled job — terminal jobs (one-shots whose target instant
    /// already passed) are auto-disabled.
    ///
    /// The engine starts stopped — call [`Self::start`] to spawn the
    /// tick task. Returning the receiver here mirrors
    /// [`crate::heartbeat::HeartbeatEngine::new`].
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] when the cron directory exists but is unreadable.
    /// - [`Error::Json`] / [`Error::InvalidStore`] when `jobs.json` is
    ///   present but unparseable.
    pub async fn new(
        cfg: &CronConfig,
        data_dir: &Path,
    ) -> Result<(Self, mpsc::Receiver<CronTick>)> {
        let store = Store::new(&data_dir.join(CRON_SUBDIR));
        let mut file = store.load().await?;
        let now = Utc::now();
        let recomputed = recompute_next_runs(&mut file.jobs, now);

        // Persist immediately so any startup auto-disables don't get
        // recomputed on every boot. Skip the I/O when nothing changed
        // — first-boot empty store is the common case.
        if recomputed > 0 {
            store.save(&file).await?;
        }

        let (tick_tx, tick_rx) = mpsc::channel(TICK_QUEUE_CAPACITY);
        let engine = Self {
            state: Arc::new(AsyncMutex::new(EngineState {
                jobs: file.jobs,
                handle: None,
            })),
            wake: Arc::new(Notify::new()),
            store,
            tick_tx,
            enabled: cfg.enabled,
        };
        Ok((engine, tick_rx))
    }

    /// Spawn the background tick task. No-op when the engine was
    /// constructed with `enabled = false` or when a task already
    /// runs.
    pub async fn start(self: &Arc<Self>) {
        if !self.enabled {
            return;
        }
        let mut state = self.state.lock().await;
        if state.handle.is_some() {
            return;
        }
        let me = Arc::clone(self);
        state.handle = Some(tokio::spawn(run_tick_loop(me)));
    }

    /// Stop the tick task and wait for it to drain.
    pub async fn shutdown(&self) {
        let handle = {
            let mut state = self.state.lock().await;
            state.handle.take()
        };
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
        }
    }

    /// Register a new job. Mints a fresh UUID v7 id, computes the
    /// initial `next_run_at`, persists, and notifies the tick loop
    /// in case the new job's wake target is earlier than the
    /// currently-pending one.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] / [`Error::Json`] on persist failure.
    pub async fn add(&self, name: String, schedule: Schedule, prompt: String) -> Result<CronJob> {
        let now = Utc::now();
        let mut job = CronJob::new(name, schedule, prompt, now);
        job.state.next_run_at = job.schedule.next_after(now);
        if job.state.next_run_at.is_none() {
            // Caller asked for a one-shot already in the past — accept
            // it but keep it disabled rather than silently rejecting.
            job.enabled = false;
        }

        let mut state = self.state.lock().await;
        state.jobs.push(job.clone());
        self.persist_locked(&state).await?;
        drop(state);
        self.wake.notify_one();
        Ok(job)
    }

    /// Remove a job by id.
    ///
    /// # Errors
    ///
    /// - [`Error::JobNotFound`] when no job has this id.
    /// - [`Error::Io`] / [`Error::Json`] on persist failure.
    pub async fn remove(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let before = state.jobs.len();
        state.jobs.retain(|j| j.id != id);
        if state.jobs.len() == before {
            return Err(Error::JobNotFound(id.to_string()));
        }
        self.persist_locked(&state).await?;
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    /// Toggle a job's `enabled` flag. Recomputes `next_run_at` when
    /// flipping back on so a re-enabled job catches the next slot
    /// rather than waiting for a stale stored value.
    ///
    /// # Errors
    ///
    /// - [`Error::JobNotFound`] when no job has this id.
    /// - [`Error::Io`] / [`Error::Json`] on persist failure.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let now = Utc::now();
        let mut state = self.state.lock().await;
        let job = state
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| Error::JobNotFound(id.to_string()))?;
        job.enabled = enabled;
        job.state.next_run_at = if enabled {
            job.schedule.next_after(now)
        } else {
            None
        };
        job.updated_at = now;
        self.persist_locked(&state).await?;
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    /// Force a job to fire on the next tick by setting its
    /// `next_run_at` to "now" and waking the loop. Honors
    /// `enabled = false` — paused jobs ignore triggers.
    ///
    /// # Errors
    ///
    /// - [`Error::JobNotFound`] when no job has this id.
    /// - [`Error::Io`] / [`Error::Json`] on persist failure.
    pub async fn trigger(&self, id: &str) -> Result<()> {
        let now = Utc::now();
        let mut state = self.state.lock().await;
        let job = state
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| Error::JobNotFound(id.to_string()))?;
        if !job.enabled {
            return Ok(());
        }
        job.state.next_run_at = Some(now);
        self.persist_locked(&state).await?;
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    /// Record the outcome of a tick that the agent finished
    /// processing. Updates `last_status` / `last_error` and bumps
    /// the consecutive-failure counter; auto-disables once the
    /// counter reaches [`AUTO_DISABLE_AFTER`]. Silently ignores
    /// unknown ids — the job may have been removed while the
    /// iteration was in flight.
    ///
    /// Persist failures are logged and swallowed: the next mutation
    /// will retry, and surfacing this error to the agent's tick
    /// handler would not give it a meaningful action.
    pub async fn report_outcome(&self, id: &str, status: RunStatus, error: Option<String>) {
        let now = Utc::now();
        let mut state = self.state.lock().await;
        let Some(job) = state.jobs.iter_mut().find(|j| j.id == id) else {
            return;
        };
        match status {
            RunStatus::Succeeded => {
                job.state.last_status = Some(RunStatus::Succeeded);
                job.state.last_error = None;
                job.state.consecutive_errors = 0;
            }
            RunStatus::Failed => {
                job.state.last_status = Some(RunStatus::Failed);
                job.state.last_error = error;
                job.state.consecutive_errors = job.state.consecutive_errors.saturating_add(1);
                if job.state.consecutive_errors >= AUTO_DISABLE_AFTER {
                    job.enabled = false;
                    job.state.next_run_at = None;
                }
            }
            RunStatus::Skipped => {
                job.state.last_status = Some(RunStatus::Skipped);
            }
        }
        job.updated_at = now;
        if let Err(err) = self.persist_locked(&state).await {
            eprintln!("[cron] failed to persist outcome for {id}: {err}");
        }
    }

    /// Snapshot the engine's runtime state — what `/cron` renders.
    /// Returns full [`CronJob`] records so the formatter can pull
    /// whichever fields it needs without going through a flattened
    /// projection type.
    pub async fn status(&self) -> CronStatus {
        let state = self.state.lock().await;
        CronStatus {
            enabled: self.enabled,
            jobs: state.jobs.clone(),
        }
    }

    /// Persist the current in-memory state to disk. Caller must hold
    /// `state` to guarantee the snapshot is consistent. The lock
    /// stays held across the await so concurrent mutations can't
    /// race the I/O.
    async fn persist_locked(&self, state: &EngineState) -> Result<()> {
        let file = StoreFile {
            version: STORE_VERSION,
            jobs: state.jobs.clone(),
        };
        self.store.save(&file).await
    }
}

/// Body of the background tick task spawned by [`CronEngine::start`].
async fn run_tick_loop(engine: Arc<CronEngine>) {
    loop {
        let sleep_dur = compute_sleep(&engine).await;

        tokio::select! {
            () = sleep(sleep_dur) => {}
            () = engine.wake.notified() => {}
        }

        let due = match collect_and_advance_due(&engine).await {
            Ok(d) => d,
            Err(err) => {
                eprintln!("[cron] tick failed: {err}");
                continue;
            }
        };

        for tick in due {
            if engine.tick_tx.send(tick).await.is_err() {
                // Receiver dropped — agent loop closed, shut down.
                return;
            }
        }
    }
}

/// Compute how long the tick task should sleep before its next pass.
async fn compute_sleep(engine: &CronEngine) -> Duration {
    let state = engine.state.lock().await;
    let now = Utc::now();
    let next = state
        .jobs
        .iter()
        .filter(|j| j.enabled)
        .filter_map(|j| j.state.next_run_at)
        .min();
    drop(state);
    match next {
        Some(t) => match (t - now).to_std() {
            Ok(d) => d.min(MAX_SLEEP),
            Err(_) => Duration::ZERO,
        },
        None => MAX_SLEEP,
    }
}

/// Find every job whose `next_run_at <= now`, mint a tick for it,
/// advance its `next_run_at`, then persist the whole batch under one
/// lock acquisition.
async fn collect_and_advance_due(engine: &CronEngine) -> Result<Vec<CronTick>> {
    let now = Utc::now();
    let mut state = engine.state.lock().await;
    let mut due = Vec::new();

    for job in &mut state.jobs {
        if !job.enabled {
            continue;
        }
        let Some(due_at) = job.state.next_run_at else {
            continue;
        };
        if now < due_at {
            continue;
        }

        due.push(CronTick {
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            prompt: job.prompt.clone(),
            at: now,
        });

        job.state.last_run_at = Some(now);
        job.updated_at = now;
        if let Some(t) = job.schedule.next_after(now) {
            job.state.next_run_at = Some(t);
        } else {
            // Schedule exhausted (one-shot in the past, cron with
            // no future fires). Disable so the next tick doesn't
            // see it, and clear next_run_at for status output.
            job.enabled = false;
            job.state.next_run_at = None;
        }
    }

    if !due.is_empty() {
        engine.persist_locked(&state).await?;
    }
    Ok(due)
}

/// Walk every enabled job and replace its `next_run_at` with the
/// strict-after-`now` value its schedule produces.
///
/// Returns the number of jobs whose state changed so the caller can
/// skip the persist when nothing moved (first boot with an empty
/// store hits this path).
fn recompute_next_runs(jobs: &mut [CronJob], now: DateTime<Utc>) -> usize {
    let mut changed = 0;
    for job in jobs {
        if !job.enabled {
            continue;
        }
        let next = job.schedule.next_after(now);
        let became_terminal = next.is_none();
        if job.state.next_run_at != next {
            job.state.next_run_at = next;
            changed += 1;
        }
        if became_terminal && job.enabled {
            job.enabled = false;
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Duration as ChronoDuration;

    use super::*;

    /// Group jobs by id so per-job assertions don't depend on the
    /// store's insertion order.
    fn jobs_by_id(jobs: &[CronJob]) -> HashMap<&str, &CronJob> {
        jobs.iter().map(|j| (j.id.as_str(), j)).collect()
    }

    fn ts(rfc: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn tempdir() -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-cron-engine-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn make_job(name: &str, schedule: Schedule) -> CronJob {
        CronJob::new(
            name.into(),
            schedule,
            format!("prompt-for-{name}"),
            ts("2026-04-25T00:00:00Z"),
        )
    }

    #[test]
    fn recompute_disables_one_shot_in_the_past() {
        let mut jobs = vec![make_job(
            "expired",
            Schedule::at(ts("2020-01-01T00:00:00Z")),
        )];
        let changed = recompute_next_runs(&mut jobs, ts("2026-04-25T00:00:00Z"));
        assert!(changed > 0);
        assert!(!jobs[0].enabled);
        assert!(jobs[0].state.next_run_at.is_none());
    }

    #[test]
    fn recompute_sets_future_next_run_at_for_recurring() {
        let mut jobs = vec![make_job("daily", Schedule::cron("0 9 * * *").unwrap())];
        let now = ts("2026-04-25T08:00:00Z");
        recompute_next_runs(&mut jobs, now);
        assert_eq!(jobs[0].state.next_run_at, Some(ts("2026-04-25T09:00:00Z")));
        assert!(jobs[0].enabled);
    }

    #[tokio::test]
    async fn add_then_status_reports_the_new_job() {
        let dir = tempdir();
        let cfg = CronConfig { enabled: true };
        let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
        let job = engine
            .add(
                "summary".into(),
                Schedule::cron("0 9 * * *").unwrap(),
                "go".into(),
            )
            .await
            .unwrap();

        let status = engine.status().await;
        assert_eq!(status.jobs.len(), 1);
        assert_eq!(status.jobs[0].id, job.id);
        assert_eq!(status.jobs[0].name, "summary");
        assert!(status.jobs[0].enabled);
    }

    #[tokio::test]
    async fn remove_unknown_returns_job_not_found() {
        let dir = tempdir();
        let cfg = CronConfig { enabled: true };
        let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
        let err = engine.remove("does-not-exist").await.unwrap_err();
        assert!(matches!(err, Error::JobNotFound(_)));
    }

    #[tokio::test]
    async fn report_outcome_auto_disables_after_threshold_failures() {
        let dir = tempdir();
        let cfg = CronConfig { enabled: true };
        let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
        let job = engine
            .add(
                "flaky".into(),
                Schedule::cron("0 9 * * *").unwrap(),
                "go".into(),
            )
            .await
            .unwrap();

        for _ in 0..AUTO_DISABLE_AFTER {
            engine
                .report_outcome(&job.id, RunStatus::Failed, Some("boom".into()))
                .await;
        }
        let status = engine.status().await;
        let entry = status.jobs.iter().find(|j| j.id == job.id).unwrap();
        assert!(!entry.enabled);
        assert_eq!(entry.state.consecutive_errors, AUTO_DISABLE_AFTER);
    }

    #[tokio::test]
    async fn report_outcome_succeeded_resets_consecutive_errors() {
        let dir = tempdir();
        let cfg = CronConfig { enabled: true };
        let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
        let job = engine
            .add(
                "noisy".into(),
                Schedule::cron("0 9 * * *").unwrap(),
                "go".into(),
            )
            .await
            .unwrap();

        engine
            .report_outcome(&job.id, RunStatus::Failed, Some("flap".into()))
            .await;
        engine
            .report_outcome(&job.id, RunStatus::Succeeded, None)
            .await;
        let status = engine.status().await;
        let entry = status.jobs.iter().find(|j| j.id == job.id).unwrap();
        assert_eq!(entry.state.consecutive_errors, 0);
        assert_eq!(entry.state.last_status, Some(RunStatus::Succeeded));
    }

    #[tokio::test]
    async fn add_persists_across_reconstruct() {
        let dir = tempdir();
        let cfg = CronConfig { enabled: true };
        {
            let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
            engine
                .add(
                    "persisted".into(),
                    Schedule::cron("0 9 * * *").unwrap(),
                    "go".into(),
                )
                .await
                .unwrap();
        }
        let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
        let status = engine.status().await;
        assert_eq!(status.jobs.len(), 1);
        assert_eq!(status.jobs[0].name, "persisted");
    }

    #[tokio::test]
    async fn collect_and_advance_fires_due_jobs_and_advances_next_run() {
        let dir = tempdir();
        let cfg = CronConfig { enabled: true };
        let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
        let job = engine
            .add(
                "tick".into(),
                Schedule::every(
                    ChronoDuration::seconds(1),
                    Utc::now() - ChronoDuration::hours(1),
                )
                .unwrap(),
                "go".into(),
            )
            .await
            .unwrap();
        // `next_after` is strict-after so the freshly-added job is
        // scheduled for the *next* step, not "now". Backdate it so
        // we exercise the due path without sleeping in the test.
        {
            let mut state = engine.state.lock().await;
            let entry = state.jobs.iter_mut().find(|j| j.id == job.id).unwrap();
            entry.state.next_run_at = Some(Utc::now() - ChronoDuration::milliseconds(100));
        }

        let due = collect_and_advance_due(&engine).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].job_id, job.id);

        let state = engine.state.lock().await;
        let by_id = jobs_by_id(&state.jobs);
        let advanced = by_id.get(job.id.as_str()).unwrap();
        assert!(advanced.state.last_run_at.is_some());
        // next_run_at must be strictly in the future relative to the
        // fire instant — the strict-after rule.
        let last = advanced.state.last_run_at.unwrap();
        let next = advanced.state.next_run_at.unwrap();
        assert!(next > last);
    }

    #[tokio::test]
    async fn collect_and_advance_disables_one_shot_after_firing() {
        let dir = tempdir();
        let cfg = CronConfig { enabled: true };
        let (engine, _rx) = CronEngine::new(&cfg, &dir).await.unwrap();
        let target = Utc::now() - ChronoDuration::seconds(1);
        let job = engine
            .add("once".into(), Schedule::at(target), "go".into())
            .await
            .unwrap();
        // The add() above flagged the past-`at` job as disabled; flip
        // it back on so we can verify the engine disables it via the
        // tick path, not just the add path.
        engine.set_enabled(&job.id, true).await.unwrap();
        // set_enabled re-asks for next_run_at, which is None for a past
        // `at` — so reach in and force a fire-able value.
        {
            let mut state = engine.state.lock().await;
            let entry = state.jobs.iter_mut().find(|j| j.id == job.id).unwrap();
            entry.state.next_run_at = Some(Utc::now() - ChronoDuration::milliseconds(1));
        }

        let due = collect_and_advance_due(&engine).await.unwrap();
        assert_eq!(due.len(), 1);

        let state = engine.state.lock().await;
        let advanced = state.jobs.iter().find(|j| j.id == job.id).unwrap();
        assert!(!advanced.enabled);
        assert!(advanced.state.next_run_at.is_none());
    }
}
