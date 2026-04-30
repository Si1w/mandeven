//! Dream scheduler.
//!
//! This is the cron-coupled timing half of Dream: it emits quiet
//! background ticks on an internal schedule. The semantic review work
//! happens in [`super::run_once`], which the agent calls when a tick
//! arrives.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::error::Result;
use super::store::Store;
use super::{DreamConfig, DreamTick, DreamTickReason};
use crate::cron::Schedule;

/// Capacity of the Dream engine tick queue.
const TICK_QUEUE_CAPACITY: usize = 4;

/// Re-check at least once a minute to recover from suspend/resume.
const MAX_SLEEP: Duration = Duration::from_mins(1);

/// Internal scheduler for Dream.
pub struct DreamEngine {
    state: Arc<AsyncMutex<EngineState>>,
    schedule: Schedule,
    tick_tx: mpsc::Sender<DreamTick>,
    enabled: bool,
    run_on_startup: bool,
    store: Store,
}

struct EngineState {
    handle: Option<JoinHandle<()>>,
}

impl DreamEngine {
    /// Build a Dream scheduler and paired tick receiver.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured cron expression is invalid.
    pub fn new(
        cfg: &DreamConfig,
        project_bucket: &Path,
    ) -> Result<(Self, mpsc::Receiver<DreamTick>)> {
        let schedule = Schedule::cron(&cfg.schedule)?;
        let (tick_tx, tick_rx) = mpsc::channel(TICK_QUEUE_CAPACITY);
        Ok((
            Self {
                state: Arc::new(AsyncMutex::new(EngineState { handle: None })),
                schedule,
                tick_tx,
                enabled: cfg.enabled,
                run_on_startup: cfg.run_on_startup,
                store: Store::new(project_bucket),
            },
            tick_rx,
        ))
    }

    /// Spawn the scheduler task.
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

    /// Stop the scheduler task.
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

    /// Dream cursor store owned by this engine.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }
}

async fn run_tick_loop(engine: Arc<DreamEngine>) {
    if engine.run_on_startup
        && engine
            .tick_tx
            .send(DreamTick {
                at: Utc::now(),
                reason: DreamTickReason::Startup,
            })
            .await
            .is_err()
    {
        return;
    }

    loop {
        let now = Utc::now();
        let Some(next) = engine.schedule.next_after(now) else {
            sleep(MAX_SLEEP).await;
            continue;
        };
        let sleep_dur = match (next - now).to_std() {
            Ok(d) => d.min(MAX_SLEEP),
            Err(_) => Duration::ZERO,
        };
        sleep(sleep_dur).await;

        let now = Utc::now();
        if now < next {
            continue;
        }
        if engine
            .tick_tx
            .send(DreamTick {
                at: now,
                reason: DreamTickReason::Scheduled,
            })
            .await
            .is_err()
        {
            return;
        }
    }
}
