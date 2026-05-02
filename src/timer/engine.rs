//! Timer scheduler that consumes `~/.mandeven/timers.json`.
//!
//! The engine finds due timers, advances their next fire time before
//! dispatch, and emits [`TimerTick`]s to the agent loop. This is the
//! runtime side of the `task + timer` primitive pair and of skill
//! frontmatter such as `timers: "0 9 * * *"`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::error::Result;
use super::{Store, StoreFile, Timer, TimerTargetRef};
use crate::task;

/// Maximum sleep between timer scans.
const MAX_SLEEP: StdDuration = StdDuration::from_secs(30);

/// Bounded queue so a burst of due timers cannot grow memory without
/// limit if the agent loop is busy.
const TICK_QUEUE_CAPACITY: usize = 32;

/// Timer scheduler.
#[derive(Debug)]
pub struct TimerEngine {
    store: Store,
    tasks: task::Manager,
    project: String,
    state: Arc<AsyncMutex<EngineState>>,
    tick_tx: mpsc::Sender<TimerTick>,
}

#[derive(Debug, Default)]
struct EngineState {
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// One scheduled timer event delivered to the agent loop.
#[derive(Clone, Debug)]
pub struct TimerTick {
    /// Timer id.
    pub timer_id: String,
    /// Runtime target.
    pub target: TimerTarget,
    /// Wall-clock instant the scheduler fired the timer.
    pub at: DateTime<Utc>,
}

/// Runtime target for a timer tick.
#[derive(Clone, Debug)]
pub enum TimerTarget {
    /// A task timer for the current project.
    Task {
        /// Referenced task id.
        task_id: String,
        /// Human-readable task subject.
        task_subject: String,
        /// User-message text fed into the agent.
        prompt: String,
    },
    /// A global skill timer.
    Skill {
        /// Skill name to invoke.
        skill: String,
    },
}

impl TimerEngine {
    /// Construct a timer engine rooted at the project bucket and
    /// global mandeven data directory.
    ///
    /// The engine starts stopped; call [`Self::start`] to spawn the
    /// polling task.
    ///
    /// # Errors
    ///
    /// Returns store errors when existing JSON timers are unreadable
    /// or invalid.
    pub async fn new(
        project_bucket: &Path,
        data_dir: &Path,
    ) -> Result<(Self, mpsc::Receiver<TimerTick>)> {
        let project = project_bucket.to_string_lossy().into_owned();
        let store = Store::new(data_dir);
        let mut file = store.load().await?;
        let now = Utc::now();
        let recomputed = recompute_next_fires(&mut file.timers, now);
        if recomputed > 0 {
            store.save(&file).await?;
        }

        let (tick_tx, tick_rx) = mpsc::channel(TICK_QUEUE_CAPACITY);
        Ok((
            Self {
                store,
                tasks: task::Manager::new(project_bucket),
                project,
                state: Arc::new(AsyncMutex::new(EngineState::default())),
                tick_tx,
            },
            tick_rx,
        ))
    }

    /// Spawn the background polling task. No-op when already running.
    pub async fn start(self: &Arc<Self>) {
        let mut state = self.state.lock().await;
        if state.handle.is_some() {
            return;
        }
        let me = Arc::clone(self);
        state.handle = Some(tokio::spawn(run_tick_loop(me)));
    }

    /// Stop the background polling task and wait for it to drain.
    pub async fn shutdown(&self) {
        let handle = {
            let mut state = self.state.lock().await;
            state.handle.take()
        };
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Path to the timer JSON file.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.store.path().to_path_buf()
    }
}

async fn run_tick_loop(engine: Arc<TimerEngine>) {
    loop {
        sleep(compute_sleep(&engine).await).await;
        let due = match collect_and_advance_due(&engine).await {
            Ok(due) => due,
            Err(err) => {
                eprintln!("[timer] tick failed: {err}");
                continue;
            }
        };

        for tick in due {
            if engine.tick_tx.send(tick).await.is_err() {
                return;
            }
        }
    }
}

async fn compute_sleep(engine: &TimerEngine) -> StdDuration {
    let file = match engine.store.load().await {
        Ok(file) => file,
        Err(err) => {
            eprintln!("[timer] failed to read timers while computing sleep: {err}");
            StoreFile::default()
        }
    };
    let now = Utc::now();
    let next = file
        .timers
        .iter()
        .filter(|timer| timer.enabled && is_runnable_by_engine(timer, &engine.project))
        .filter_map(|timer| timer.next_fire_at)
        .min();
    match next {
        Some(time) => match (time - now).to_std() {
            Ok(duration) => duration.min(MAX_SLEEP),
            Err(_) => StdDuration::ZERO,
        },
        None => MAX_SLEEP,
    }
}

async fn collect_and_advance_due(engine: &TimerEngine) -> Result<Vec<TimerTick>> {
    let _guard = engine.state.lock().await;
    let now = Utc::now();
    let mut file = engine.store.load().await?;
    let mut due = Vec::new();
    let mut changed = false;

    for timer in &mut file.timers {
        if !timer.enabled || !is_runnable_by_engine(timer, &engine.project) {
            continue;
        }
        let Some(due_at) = timer.next_fire_at else {
            continue;
        };
        if now < due_at {
            continue;
        }

        match timer.target.clone() {
            TimerTargetRef::Task { task_id, .. } => {
                let Some(task) = engine.tasks.get(&task_id).await? else {
                    eprintln!(
                        "[timer] skipping timer {} because task {} is missing",
                        timer.id, task_id
                    );
                    advance_timer(timer, now);
                    changed = true;
                    continue;
                };

                due.push(TimerTick {
                    timer_id: timer.id.clone(),
                    target: TimerTarget::Task {
                        task_id: task.id.clone(),
                        task_subject: task.subject.clone(),
                        prompt: prompt_for_task(timer, &task),
                    },
                    at: now,
                });
            }
            TimerTargetRef::Skill { skill } => {
                due.push(TimerTick {
                    timer_id: timer.id.clone(),
                    target: TimerTarget::Skill { skill },
                    at: now,
                });
            }
        }
        advance_timer(timer, now);
        changed = true;
    }

    if changed {
        engine.store.save(&file).await?;
    }
    Ok(due)
}

fn advance_timer(timer: &mut Timer, now: DateTime<Utc>) {
    timer.last_fire_at = Some(now);
    timer.updated_at = now;
    if let Some(next) = timer.schedule.next_after(now) {
        timer.next_fire_at = Some(next);
    } else {
        timer.enabled = false;
        timer.next_fire_at = None;
    }
}

fn recompute_next_fires(timers: &mut [Timer], now: DateTime<Utc>) -> usize {
    let mut changed = 0;
    for timer in timers {
        if !timer.enabled {
            continue;
        }
        let next = timer.schedule.next_after(now);
        if timer.next_fire_at != next {
            timer.next_fire_at = next;
            timer.updated_at = now;
            changed += 1;
        }
        if next.is_none() {
            timer.enabled = false;
            timer.updated_at = now;
            changed += 1;
        }
    }
    changed
}

fn is_runnable_by_engine(timer: &Timer, project: &str) -> bool {
    match &timer.target {
        TimerTargetRef::Task {
            project: timer_project,
            ..
        } => timer_project == project,
        TimerTargetRef::Skill { .. } => true,
    }
}

fn prompt_for_task(timer: &Timer, task: &task::Task) -> String {
    format!(
        "# Scheduled task: {}\n\nTimer ID: {}\nTask ID: {}\n\n{}",
        task.subject,
        timer.id,
        task.id,
        task.description.trim()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Duration;

    use super::super::store::StoreFile;
    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-timer-engine-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn task_draft(subject: &str) -> task::TaskDraft {
        task::TaskDraft {
            subject: subject.to_string(),
            description: format!("Do {subject}"),
            active_form: None,
            owner: None,
            metadata: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn collect_due_timer_advances_state_and_emits_task_prompt() {
        let dir = tempdir();
        let task = task::Manager::new(&dir)
            .create(task_draft("check build"))
            .await
            .unwrap();
        let (engine, _rx) = TimerEngine::new(&dir, &dir).await.unwrap();
        let mut file = StoreFile::default();
        let now = Utc::now();
        file.timers.push(Timer {
            id: uuid::Uuid::now_v7().to_string(),
            target: TimerTargetRef::Task {
                project: dir.to_string_lossy().into_owned(),
                task_id: task.id.clone(),
            },
            enabled: true,
            schedule: super::super::Schedule::every(Duration::minutes(5), now - Duration::hours(1))
                .unwrap(),
            next_fire_at: Some(now - Duration::seconds(1)),
            last_fire_at: None,
            created_at: now,
            updated_at: now,
        });
        engine.store.save(&file).await.unwrap();

        let due = collect_and_advance_due(&engine).await.unwrap();
        assert_eq!(due.len(), 1);
        let TimerTarget::Task {
            task_id, prompt, ..
        } = &due[0].target
        else {
            panic!("expected task timer tick");
        };
        assert_eq!(task_id, &task.id);
        assert!(prompt.contains("Do check build"));

        let loaded = engine.store.load().await.unwrap();
        assert!(loaded.timers[0].last_fire_at.is_some());
        assert!(loaded.timers[0].next_fire_at.unwrap() > now);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[test]
    fn recompute_disables_expired_one_shot() {
        let now = Utc::now();
        let mut timers = vec![Timer {
            id: uuid::Uuid::now_v7().to_string(),
            target: TimerTargetRef::Skill {
                skill: "heartbeat".to_string(),
            },
            enabled: true,
            schedule: super::super::Schedule::at(now - Duration::minutes(1)),
            next_fire_at: Some(now - Duration::minutes(1)),
            last_fire_at: None,
            created_at: now,
            updated_at: now,
        }];

        assert!(recompute_next_fires(&mut timers, now) > 0);
        assert!(!timers[0].enabled);
        assert!(timers[0].next_fire_at.is_none());
    }
}
