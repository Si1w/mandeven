//! Global timers used by model-facing timer tools and skill timers.
//!
//! A timer is machine-readable JSON state that binds a schedule to a
//! small target reference. Task timers point at project-local task
//! Markdown; skill timers point at editable skill definitions. Cron-style
//! scheduled work is represented as `task + timer`, not as a separate
//! high-level cron primitive.

pub mod engine;
pub mod error;
pub mod schedule;
pub mod store;

pub use engine::{TimerEngine, TimerTarget, TimerTick};
pub use error::{Error, Result};
pub use schedule::{Schedule, ScheduleError};
pub use store::{GLOBAL_TIMER_FILENAME, Store, StoreFile};

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::skill::SkillIndex;
use crate::task;

/// Timer lifecycle state and schedule binding.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Timer {
    /// UUID v7 stable machine id.
    pub id: String,
    /// Small reference to the thing fired by this timer.
    pub target: TimerTargetRef,
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

/// Runtime target reference persisted in `timers.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimerTargetRef {
    /// A task in one project bucket.
    Task {
        /// Project bucket path that owns the task Markdown.
        project: String,
        /// Referenced task id.
        task_id: String,
    },
    /// A skill name from the loaded skill index.
    Skill {
        /// Referenced skill name.
        skill: String,
    },
}

impl TimerTargetRef {
    /// Return the referenced task id when this target is a task.
    #[must_use]
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::Task { task_id, .. } => Some(task_id),
            Self::Skill { .. } => None,
        }
    }

    /// Return the referenced skill name when this target is a skill.
    #[must_use]
    pub fn skill_name(&self) -> Option<&str> {
        match self {
            Self::Skill { skill } => Some(skill),
            Self::Task { .. } => None,
        }
    }
}

/// Input for creating a task timer.
#[derive(Clone, Debug)]
pub struct TimerDraft {
    /// Referenced task id.
    pub task_id: String,
    /// Schedule rule.
    pub schedule: Schedule,
}

/// Partial update for a task timer.
#[derive(Clone, Debug, Default)]
pub struct TimerUpdate {
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

/// Project-scoped view over the global timer store.
#[derive(Debug)]
pub struct Manager {
    store: Store,
    tasks: task::Manager,
    project: String,
    lock: Mutex<()>,
}

impl Manager {
    /// Construct a timer manager backed by the global data directory
    /// while exposing only task timers for the given project bucket.
    #[must_use]
    pub fn new(data_dir: &Path, project_bucket: &Path) -> Self {
        Self {
            store: Store::new(data_dir),
            tasks: task::Manager::new(project_bucket),
            project: project_key(project_bucket),
            lock: Mutex::new(()),
        }
    }

    /// Path to the backing JSON store.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Create a new task timer after validating the referenced task.
    ///
    /// # Errors
    ///
    /// Returns validation or store I/O errors.
    pub async fn create(&self, draft: TimerDraft) -> Result<Timer> {
        validate_task_exists(&self.tasks, &draft.task_id).await?;

        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let now = Utc::now();
        let timer = Timer {
            id: Uuid::now_v7().to_string(),
            target: TimerTargetRef::Task {
                project: self.project.clone(),
                task_id: draft.task_id,
            },
            enabled: true,
            next_fire_at: draft.schedule.next_after(now),
            last_fire_at: None,
            schedule: draft.schedule,
            created_at: now,
            updated_at: now,
        };
        file.timers.push(timer.clone());
        self.store.save(&file).await?;
        Ok(timer)
    }

    /// Read a current-project task timer by id.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn get(&self, id: &str) -> Result<Option<Timer>> {
        let file = self.store.load().await?;
        Ok(file
            .timers
            .into_iter()
            .find(|timer| self.is_current_project_task_timer(timer) && timer.id == id))
    }

    /// List current-project task timers sorted by creation time.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn list(&self) -> Result<Vec<Timer>> {
        let mut timers: Vec<Timer> = self
            .store
            .load()
            .await?
            .timers
            .into_iter()
            .filter(|timer| self.is_current_project_task_timer(timer))
            .collect();
        sort_timers(&mut timers);
        Ok(timers)
    }

    /// Apply a partial task timer update.
    ///
    /// Returns `Ok(None)` when the target timer does not exist in the
    /// current project view.
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
        let Some(index) = find_task_timer_index(&file.timers, id, &self.project) else {
            return Ok(None);
        };
        let fields = apply_update(&mut file.timers[index], update);
        if !fields.is_empty() {
            let now = Utc::now();
            file.timers[index].updated_at = now;
            if fields
                .iter()
                .any(|field| field == "schedule" || field == "enabled")
            {
                file.timers[index].next_fire_at = file.timers[index]
                    .enabled
                    .then(|| file.timers[index].schedule.next_after(now))
                    .flatten();
            }
            self.store.save(&file).await?;
        }
        Ok(Some(UpdateOutcome {
            timer: file.timers[index].clone(),
            updated_fields: fields,
        }))
    }

    /// Delete a current-project task timer.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let before = file.timers.len();
        file.timers
            .retain(|timer| !(timer.id == id && self.is_current_project_task_timer(timer)));
        if file.timers.len() == before {
            return Ok(false);
        }
        self.store.save(&file).await?;
        Ok(true)
    }

    /// Mark a task timer as fired now and return the referenced task.
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
        let Some(index) = find_task_timer_index(&file.timers, id, &self.project) else {
            return Ok(None);
        };
        let Some(task_id) = file.timers[index].target.task_id().map(ToOwned::to_owned) else {
            return Ok(None);
        };
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
        Ok(Some(FireOutcome {
            timer: file.timers[index].clone(),
            task,
        }))
    }

    fn is_current_project_task_timer(&self, timer: &Timer) -> bool {
        is_task_timer_for_project(timer, &self.project)
    }
}

/// Sync skill-declared timers into the global timer store.
///
/// The declaration is intentionally tiny: `timers: "0 9 * * *"`.
/// Discovery upserts by skill name while preserving UUID timer ids.
///
/// # Errors
///
/// Returns timer store I/O or JSON errors when the global timer file
/// cannot be loaded or saved.
pub async fn sync_skill_timers(data_dir: &Path, skills: &SkillIndex) -> Result<()> {
    let store = Store::new(data_dir);
    let mut file = store.load().await?;
    let now = Utc::now();
    let mut desired_skills = BTreeSet::new();
    let mut changed = false;

    for skill in skills.skills() {
        let Some(expr) = skill.frontmatter.timers.as_deref() else {
            continue;
        };
        let expr = expr.trim();
        if expr.is_empty() {
            continue;
        }
        let schedule = match Schedule::cron(expr) {
            Ok(schedule) => schedule,
            Err(err) => {
                eprintln!(
                    "[timer] skipped invalid timer on skill /{}: {err}",
                    skill.frontmatter.name
                );
                continue;
            }
        };
        let skill_name = skill.frontmatter.name.clone();
        desired_skills.insert(skill_name.clone());
        let next_fire_at = schedule.next_after(now);

        if let Some(existing) = find_skill_timer_mut(&mut file.timers, &skill_name) {
            if !cron_expr_matches(&existing.schedule, expr) {
                existing.schedule = schedule;
                existing.next_fire_at = next_fire_at;
                existing.updated_at = now;
                changed = true;
            }
            continue;
        }

        file.timers.push(Timer {
            id: Uuid::now_v7().to_string(),
            target: TimerTargetRef::Skill { skill: skill_name },
            enabled: true,
            schedule,
            next_fire_at,
            last_fire_at: None,
            created_at: now,
            updated_at: now,
        });
        changed = true;
    }

    let before = file.timers.len();
    file.timers.retain(|timer| {
        !matches!(
            &timer.target,
            TimerTargetRef::Skill { skill } if !desired_skills.contains(skill)
        )
    });
    changed |= file.timers.len() != before;

    if changed {
        sort_timers(&mut file.timers);
        store.save(&file).await?;
    }
    Ok(())
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
    if let Some(value) = update.task_id.as_deref() {
        validate_text("task_id", value)?;
    }
    Ok(())
}

fn apply_update(timer: &mut Timer, update: TimerUpdate) -> Vec<String> {
    let mut fields = Vec::new();
    if let Some(task_id) = update.task_id {
        let current = timer.target.task_id();
        if current != Some(task_id.as_str())
            && let TimerTargetRef::Task {
                task_id: existing, ..
            } = &mut timer.target
        {
            *existing = task_id;
            fields.push("task_id".to_string());
        }
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

fn find_task_timer_index(timers: &[Timer], id: &str, project: &str) -> Option<usize> {
    timers
        .iter()
        .position(|timer| timer.id == id && is_task_timer_for_project(timer, project))
}

fn find_skill_timer_mut<'a>(timers: &'a mut [Timer], skill_name: &str) -> Option<&'a mut Timer> {
    timers.iter_mut().find(|timer| {
        matches!(
            &timer.target,
            TimerTargetRef::Skill { skill } if skill == skill_name
        )
    })
}

fn is_task_timer_for_project(timer: &Timer, project_key: &str) -> bool {
    matches!(
        &timer.target,
        TimerTargetRef::Task { project, .. } if project == project_key
    )
}

fn cron_expr_matches(schedule: &Schedule, expr: &str) -> bool {
    matches!(schedule, Schedule::Cron { expr: current, .. } if current == expr)
}

fn sort_timers(timers: &mut [Timer]) {
    timers.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn project_key(project_bucket: &Path) -> String {
    project_bucket.to_string_lossy().into_owned()
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use chrono::Duration;

    use crate::skill::{Skill, SkillFrontmatter, SkillIndex};

    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("mandeven-{name}-{}", uuid::Uuid::now_v7()));
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

    fn skill(name: &str, timers: Option<&str>) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: name.to_string(),
                description: "desc".to_string(),
                allowed_tools: Vec::new(),
                timers: timers.map(ToString::to_string),
                fork: false,
            },
            body: String::new(),
            source_path: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
        }
    }

    #[tokio::test]
    async fn create_validates_task_and_writes_global_json() {
        let dir = tempdir("timer-test");
        let tasks = task::Manager::new(&dir);
        let task = tasks.create(task_draft("paper progress")).await.unwrap();
        let manager = Manager::new(&dir, &dir);

        let timer = manager
            .create(TimerDraft {
                task_id: task.id.clone(),
                schedule: Schedule::cron("0 9 * * *").unwrap(),
            })
            .await
            .unwrap();

        assert!(Uuid::parse_str(&timer.id).is_ok());
        assert_eq!(timer.target.task_id(), Some(task.id.as_str()));
        assert!(dir.join(GLOBAL_TIMER_FILENAME).exists());
        assert!(!dir.join("timers").exists());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn fire_now_updates_state_and_returns_task() {
        let dir = tempdir("timer-test");
        let tasks = task::Manager::new(&dir);
        let task = tasks.create(task_draft("check build")).await.unwrap();
        let manager = Manager::new(&dir, &dir);
        let timer = manager
            .create(TimerDraft {
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

    #[tokio::test]
    async fn sync_skill_timers_upserts_uuid_json() {
        let dir = tempdir("global-timer");
        let skills = SkillIndex::from_skills(vec![skill("cron", Some("0 9 * * *"))]);
        sync_skill_timers(&dir, &skills).await.unwrap();

        let file = Store::new(&dir).load().await.unwrap();
        assert_eq!(file.timers.len(), 1);
        assert!(Uuid::parse_str(&file.timers[0].id).is_ok());
        assert_eq!(file.timers[0].target.skill_name(), Some("cron"));
        assert!(cron_expr_matches(&file.timers[0].schedule, "0 9 * * *"));

        let first_id = file.timers[0].id.clone();
        sync_skill_timers(&dir, &skills).await.unwrap();
        let file = Store::new(&dir).load().await.unwrap();
        assert_eq!(file.timers[0].id, first_id);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
