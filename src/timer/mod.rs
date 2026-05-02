//! Project-local timers used by model-facing timer tools.
//!
//! A timer is declarative Markdown state that points at a task and a
//! schedule. Cron-style scheduled work is represented as `task +
//! timer`; the scheduler/runner layer can later consume this state and
//! invoke `task.run` without exposing cron as a separate primitive.

pub mod engine;
pub mod error;
pub mod global;
pub mod schedule;
pub mod store;

pub use engine::{TimerEngine, TimerTarget, TimerTick};
pub use error::{Error, Result};
pub use global::{GLOBAL_TIMER_FILENAME, GlobalStore, sync_skill_timers};
pub use schedule::{Schedule, ScheduleError};
pub use store::{Store, StoreFile};

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::task;

/// Subdirectory under a project bucket holding timer Markdown files.
pub const TIMER_SUBDIR: &str = "timers";

/// Timer lifecycle state and schedule binding.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Timer {
    /// UUID v7 stable machine id.
    pub id: String,
    /// User-readable Markdown path relative to the project bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Human-readable title rendered in the Markdown heading.
    pub title: String,
    /// Referenced task id. References use ids, not filenames.
    pub task_id: String,
    /// `false` mutes the timer without deleting it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Schedule rule for this timer.
    pub schedule: Schedule,
    /// Next computed firing instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<DateTime<Utc>>,
    /// Most recent manual or scheduler fire instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last mutation time.
    pub updated_at: DateTime<Utc>,
}

/// Input for creating a timer.
#[derive(Clone, Debug)]
pub struct TimerDraft {
    /// Human-readable title.
    pub title: String,
    /// Referenced task id.
    pub task_id: String,
    /// Schedule rule.
    pub schedule: Schedule,
}

/// Partial update for a timer.
#[derive(Clone, Debug, Default)]
pub struct TimerUpdate {
    /// Replacement title.
    pub title: Option<String>,
    /// Replacement referenced task id.
    pub task_id: Option<String>,
    /// Replacement enabled flag.
    pub enabled: Option<bool>,
    /// Replacement schedule.
    pub schedule: Option<Schedule>,
}

/// Outcome of a successful timer update.
#[derive(Clone, Debug)]
pub struct UpdateOutcome {
    /// Updated timer.
    pub timer: Timer,
    /// Names of fields that changed.
    pub updated_fields: Vec<String>,
}

/// Outcome of a manual timer fire.
#[derive(Clone, Debug)]
pub struct FireOutcome {
    /// Updated timer.
    pub timer: Timer,
    /// Referenced task that should be run by the caller/scheduler.
    pub task: task::Task,
}

/// Project-local timer manager.
#[derive(Debug)]
pub struct Manager {
    store: Store,
    tasks: task::Manager,
    lock: Mutex<()>,
}

impl Manager {
    /// Construct a timer manager rooted at the given project bucket.
    #[must_use]
    pub fn new(project_bucket: &Path) -> Self {
        Self {
            store: Store::new(&project_bucket.join(TIMER_SUBDIR)),
            tasks: task::Manager::new(project_bucket),
            lock: Mutex::new(()),
        }
    }

    /// Path to the backing store directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Create a new timer after validating the referenced task.
    ///
    /// # Errors
    ///
    /// Returns validation or store I/O errors.
    pub async fn create(&self, draft: TimerDraft) -> Result<Timer> {
        validate_text("title", &draft.title)?;
        validate_task_exists(&self.tasks, &draft.task_id).await?;

        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let now = Utc::now();
        let timer = Timer {
            id: Uuid::now_v7().to_string(),
            path: None,
            title: draft.title,
            task_id: draft.task_id,
            enabled: true,
            next_fire_at: draft.schedule.next_after(now),
            last_fire_at: None,
            schedule: draft.schedule,
            created_at: now,
            updated_at: now,
        };
        file.timers.push(timer.clone());
        self.store.save(&file).await?;
        Ok(self
            .store
            .load()
            .await?
            .timers
            .into_iter()
            .find(|stored| stored.id == timer.id)
            .unwrap_or(timer))
    }

    /// Read a timer by id.
    ///
    /// # Errors
    ///
    /// Returns store I/O or TOML errors.
    pub async fn get(&self, id: &str) -> Result<Option<Timer>> {
        let file = self.store.load().await?;
        Ok(file.timers.into_iter().find(|timer| timer.id == id))
    }

    /// List every timer sorted by creation time.
    ///
    /// # Errors
    ///
    /// Returns store I/O or TOML errors.
    pub async fn list(&self) -> Result<Vec<Timer>> {
        let mut timers = self.store.load().await?.timers;
        sort_timers(&mut timers);
        Ok(timers)
    }

    /// Apply a partial timer update.
    ///
    /// Returns `Ok(None)` when the target timer does not exist.
    ///
    /// # Errors
    ///
    /// Returns validation, referenced-task, or store errors.
    pub async fn update(&self, id: &str, update: TimerUpdate) -> Result<Option<UpdateOutcome>> {
        validate_update(&update)?;
        if let Some(task_id) = update.task_id.as_deref() {
            validate_task_exists(&self.tasks, task_id).await?;
        }

        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let Some(index) = find_timer_index(&file.timers, id) else {
            return Ok(None);
        };
        let fields = apply_update(&mut file.timers[index], update);
        if !fields.is_empty() {
            file.timers[index].updated_at = Utc::now();
            if fields
                .iter()
                .any(|field| field == "schedule" || field == "enabled")
            {
                let now = Utc::now();
                file.timers[index].next_fire_at = file.timers[index]
                    .enabled
                    .then(|| file.timers[index].schedule.next_after(now))
                    .flatten();
            }
            self.store.save(&file).await?;
            let updated_id = file.timers[index].id.clone();
            file = self.store.load().await?;
            let Some(updated_index) = find_timer_index(&file.timers, &updated_id) else {
                return Err(Error::TimerNotFound(updated_id));
            };
            return Ok(Some(UpdateOutcome {
                timer: file.timers[updated_index].clone(),
                updated_fields: fields,
            }));
        }
        Ok(Some(UpdateOutcome {
            timer: file.timers[index].clone(),
            updated_fields: fields,
        }))
    }

    /// Delete a timer.
    ///
    /// # Errors
    ///
    /// Returns store I/O or TOML errors.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let before = file.timers.len();
        file.timers.retain(|timer| timer.id != id);
        if file.timers.len() == before {
            return Ok(false);
        }
        self.store.save(&file).await?;
        Ok(true)
    }

    /// Mark a timer as fired now and return the referenced task.
    ///
    /// This updates timer state only. The caller/scheduler is
    /// responsible for invoking the task runner and recording run
    /// JSONL.
    ///
    /// # Errors
    ///
    /// Returns validation, referenced-task, or store errors.
    pub async fn fire_now(&self, id: &str) -> Result<Option<FireOutcome>> {
        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let Some(index) = find_timer_index(&file.timers, id) else {
            return Ok(None);
        };
        let task_id = file.timers[index].task_id.clone();
        let Some(task) = self.tasks.get(&task_id).await? else {
            return Err(Error::TaskNotFound(task_id));
        };
        let now = Utc::now();
        file.timers[index].last_fire_at = Some(now);
        file.timers[index].next_fire_at = file.timers[index]
            .enabled
            .then(|| file.timers[index].schedule.next_after(now))
            .flatten();
        file.timers[index].updated_at = now;
        self.store.save(&file).await?;
        let fired_id = file.timers[index].id.clone();
        file = self.store.load().await?;
        let Some(updated_index) = find_timer_index(&file.timers, &fired_id) else {
            return Err(Error::TimerNotFound(fired_id));
        };
        Ok(Some(FireOutcome {
            timer: file.timers[updated_index].clone(),
            task,
        }))
    }
}

async fn validate_task_exists(tasks: &task::Manager, id: &str) -> Result<()> {
    validate_text("task_id", id)?;
    if tasks.get(id).await?.is_none() {
        return Err(Error::TaskNotFound(id.to_string()));
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::InvalidField {
            field,
            message: "must not be empty".to_string(),
        });
    }
    Ok(())
}

fn validate_update(update: &TimerUpdate) -> Result<()> {
    if let Some(value) = update.title.as_deref() {
        validate_text("title", value)?;
    }
    if let Some(value) = update.task_id.as_deref() {
        validate_text("task_id", value)?;
    }
    Ok(())
}

fn apply_update(timer: &mut Timer, update: TimerUpdate) -> Vec<String> {
    let mut fields = Vec::new();
    if let Some(title) = update.title
        && timer.title != title
    {
        timer.title = title;
        fields.push("title".to_string());
    }
    if let Some(task_id) = update.task_id
        && timer.task_id != task_id
    {
        timer.task_id = task_id;
        fields.push("task_id".to_string());
    }
    if let Some(enabled) = update.enabled
        && timer.enabled != enabled
    {
        timer.enabled = enabled;
        fields.push("enabled".to_string());
    }
    if let Some(schedule) = update.schedule {
        timer.schedule = schedule;
        fields.push("schedule".to_string());
    }
    fields
}

fn find_timer_index(timers: &[Timer], id: &str) -> Option<usize> {
    timers.iter().position(|timer| timer.id == id)
}

fn sort_timers(timers: &mut [Timer]) {
    timers.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use chrono::Duration;

    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-timer-test-{}", uuid::Uuid::now_v7()));
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
    async fn create_validates_task_and_writes_markdown() {
        let dir = tempdir();
        let tasks = task::Manager::new(&dir);
        let task = tasks.create(task_draft("paper progress")).await.unwrap();
        let manager = Manager::new(&dir);

        let timer = manager
            .create(TimerDraft {
                title: "Daily paper progress".to_string(),
                task_id: task.id,
                schedule: Schedule::cron("0 9 * * *").unwrap(),
            })
            .await
            .unwrap();

        assert!(uuid::Uuid::parse_str(&timer.id).is_ok());
        assert_eq!(
            timer.path.as_deref(),
            Some("timers/daily-paper-progress.md")
        );
        assert!(dir.join("timers").join("daily-paper-progress.md").exists());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn fire_now_updates_state_and_returns_task() {
        let dir = tempdir();
        let tasks = task::Manager::new(&dir);
        let task = tasks.create(task_draft("check build")).await.unwrap();
        let manager = Manager::new(&dir);
        let timer = manager
            .create(TimerDraft {
                title: "Build check".to_string(),
                task_id: task.id.clone(),
                schedule: Schedule::every(Duration::minutes(5), Utc::now()).unwrap(),
            })
            .await
            .unwrap();

        let fired = manager.fire_now(&timer.id).await.unwrap().unwrap();
        assert_eq!(fired.task.id, task.id);
        assert!(fired.timer.last_fire_at.is_some());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
