//! Project-local task list used by model-facing task tools.
//!
//! This module intentionally models tasks rather than a visual Kanban
//! board. Columns are a view over task state:
//! `pending`, `in_progress`, `blocked` (derived from unresolved
//! dependencies), and `completed`. The shape mirrors Claude Code's
//! newer task tools: tasks are durable, optionally owned by an agent,
//! and can express dependencies through `blocks` / `blocked_by`.

pub mod error;
pub mod store;

pub use error::{Error, Result};
pub use store::{Store, StoreFile};

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Subdirectory under a project bucket holding the task store.
pub const TASK_SUBDIR: &str = "tasks";

/// Legacy filename inside [`TASK_SUBDIR`] holding task definitions.
///
/// New task stores are one Markdown file per task. The JSON file is
/// still read so existing installs migrate on the next write.
pub const TASK_STORE_FILENAME: &str = "tasks.json";

/// Current task store schema version.
pub const STORE_VERSION: u32 = 1;

/// Task lifecycle state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not started yet.
    #[default]
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished successfully.
    Completed,
}

/// One project-local task.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Task {
    /// UUID v7 stable machine id, represented as a string so tools can
    /// refer to ids uniformly in JSON.
    pub id: String,
    /// User-readable Markdown path relative to the project bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Brief actionable title.
    pub subject: String,
    /// Full description of what needs to be done.
    pub description: String,
    /// Present-continuous text for status displays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    /// Agent name/id currently responsible for the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Lifecycle status.
    pub status: TaskStatus,
    /// Task ids this task blocks.
    #[serde(default)]
    pub blocks: Vec<String>,
    /// Task ids that must complete before this task can proceed.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// Free-form metadata for future multi-agent coordination.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last mutation time.
    pub updated_at: DateTime<Utc>,
}

/// Input for creating a task.
#[derive(Clone, Debug)]
pub struct TaskDraft {
    /// Brief actionable title.
    pub subject: String,
    /// Full description.
    pub description: String,
    /// Present-continuous text for status displays.
    pub active_form: Option<String>,
    /// Optional initial owner.
    pub owner: Option<String>,
    /// Optional metadata.
    pub metadata: BTreeMap<String, Value>,
}

/// Partial update for a task.
#[derive(Clone, Debug, Default)]
pub struct TaskUpdate {
    /// Replacement subject.
    pub subject: Option<String>,
    /// Replacement description.
    pub description: Option<String>,
    /// Replacement active form.
    pub active_form: OptionalTextUpdate,
    /// Replacement owner.
    pub owner: OptionalTextUpdate,
    /// Replacement lifecycle status.
    pub status: Option<TaskStatus>,
    /// Metadata merge. Values set to `null` delete that key.
    pub metadata: Option<BTreeMap<String, Value>>,
    /// Task ids this task should block.
    pub add_blocks: Vec<String>,
    /// Task ids that should block this task.
    pub add_blocked_by: Vec<String>,
}

/// Update operation for an optional string field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum OptionalTextUpdate {
    /// Leave the field unchanged.
    #[default]
    Unchanged,
    /// Set the field to the provided value.
    Set(String),
    /// Clear the field.
    Clear,
}

/// Outcome of a successful task update.
#[derive(Clone, Debug)]
pub struct UpdateOutcome {
    /// Updated task.
    pub task: Task,
    /// Names of fields that changed.
    pub updated_fields: Vec<String>,
}

/// Project-local task manager.
#[derive(Debug)]
pub struct Manager {
    store: Store,
    lock: Mutex<()>,
}

impl Manager {
    /// Construct a task manager rooted at the given project bucket.
    #[must_use]
    pub fn new(project_bucket: &Path) -> Self {
        Self {
            store: Store::new(&project_bucket.join(TASK_SUBDIR)),
            lock: Mutex::new(()),
        }
    }

    /// Path to the backing store directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Create a new task.
    ///
    /// # Errors
    ///
    /// Returns validation or store I/O errors.
    pub async fn create(&self, draft: TaskDraft) -> Result<Task> {
        validate_text("subject", &draft.subject)?;
        validate_text("description", &draft.description)?;
        if let Some(active) = draft.active_form.as_deref() {
            validate_text("active_form", active)?;
        }
        if let Some(owner) = draft.owner.as_deref() {
            validate_text("owner", owner)?;
        }

        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let now = Utc::now();
        let task = Task {
            id: Uuid::now_v7().to_string(),
            path: None,
            subject: draft.subject,
            description: draft.description,
            active_form: draft.active_form,
            owner: draft.owner,
            status: TaskStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: draft.metadata,
            created_at: now,
            updated_at: now,
        };
        file.tasks.push(task.clone());
        self.store.save(&file).await?;
        Ok(self
            .store
            .load()
            .await?
            .tasks
            .into_iter()
            .find(|stored| stored.id == task.id)
            .unwrap_or(task))
    }

    /// Read a task by id.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn get(&self, id: &str) -> Result<Option<Task>> {
        let file = self.store.load().await?;
        Ok(file.tasks.into_iter().find(|task| task.id == id))
    }

    /// List every task sorted by creation time.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn list(&self) -> Result<Vec<Task>> {
        let mut tasks = self.store.load().await?.tasks;
        sort_tasks(&mut tasks);
        Ok(tasks)
    }

    /// Apply a partial update and dependency additions.
    ///
    /// Returns `Ok(None)` when the target task does not exist.
    ///
    /// # Errors
    ///
    /// Returns validation, dependency, or store errors.
    pub async fn update(&self, id: &str, update: TaskUpdate) -> Result<Option<UpdateOutcome>> {
        validate_update(&update)?;

        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let Some(index) = find_task_index(&file.tasks, id) else {
            return Ok(None);
        };
        validate_dependency_ids(&file.tasks, id, &update)?;

        let mut fields = apply_basic_update(&mut file.tasks[index], &update);
        fields.extend(apply_dependency_update(&mut file.tasks, index, &update)?);
        if !fields.is_empty() {
            file.tasks[index].updated_at = Utc::now();
            self.store.save(&file).await?;
            let updated_id = file.tasks[index].id.clone();
            file = self.store.load().await?;
            let Some(updated_index) = find_task_index(&file.tasks, &updated_id) else {
                return Err(Error::TaskNotFound(updated_id));
            };
            return Ok(Some(UpdateOutcome {
                task: file.tasks[updated_index].clone(),
                updated_fields: fields,
            }));
        }
        Ok(Some(UpdateOutcome {
            task: file.tasks[index].clone(),
            updated_fields: fields,
        }))
    }

    /// Delete a task and remove dependency references to it.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let mut file = self.store.load().await?;
        let before = file.tasks.len();
        file.tasks.retain(|task| task.id != id);
        if file.tasks.len() == before {
            return Ok(false);
        }
        for task in &mut file.tasks {
            task.blocks.retain(|block| block != id);
            task.blocked_by.retain(|blocker| blocker != id);
        }
        self.store.save(&file).await?;
        Ok(true)
    }
}

/// Return unresolved blockers for `task`.
#[must_use]
pub fn unresolved_blockers(task: &Task, all_tasks: &[Task]) -> Vec<String> {
    let unresolved: HashSet<&str> = all_tasks
        .iter()
        .filter(|candidate| candidate.status != TaskStatus::Completed)
        .map(|candidate| candidate.id.as_str())
        .collect();
    task.blocked_by
        .iter()
        .filter(|id| unresolved.contains(id.as_str()))
        .cloned()
        .collect()
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

fn validate_update(update: &TaskUpdate) -> Result<()> {
    if let Some(value) = update.subject.as_deref() {
        validate_text("subject", value)?;
    }
    if let Some(value) = update.description.as_deref() {
        validate_text("description", value)?;
    }
    if let OptionalTextUpdate::Set(value) = &update.active_form {
        validate_text("active_form", value)?;
    }
    if let OptionalTextUpdate::Set(value) = &update.owner {
        validate_text("owner", value)?;
    }
    Ok(())
}

fn find_task_index(tasks: &[Task], id: &str) -> Option<usize> {
    tasks.iter().position(|task| task.id == id)
}

fn validate_dependency_ids(tasks: &[Task], id: &str, update: &TaskUpdate) -> Result<()> {
    for dependency in update.add_blocks.iter().chain(&update.add_blocked_by) {
        if dependency == id {
            return Err(Error::SelfDependency(id.to_string()));
        }
        if find_task_index(tasks, dependency).is_none() {
            return Err(Error::TaskNotFound(dependency.clone()));
        }
    }
    Ok(())
}

fn apply_basic_update(task: &mut Task, update: &TaskUpdate) -> Vec<String> {
    let mut fields = Vec::new();
    set_string(
        &mut task.subject,
        update.subject.as_ref(),
        "subject",
        &mut fields,
    );
    set_string(
        &mut task.description,
        update.description.as_ref(),
        "description",
        &mut fields,
    );
    set_option_string(
        &mut task.active_form,
        &update.active_form,
        "active_form",
        &mut fields,
    );
    set_option_string(&mut task.owner, &update.owner, "owner", &mut fields);
    if let Some(status) = update.status
        && task.status != status
    {
        task.status = status;
        fields.push("status".to_string());
    }
    if let Some(metadata) = &update.metadata {
        merge_metadata(&mut task.metadata, metadata, &mut fields);
    }
    fields
}

fn set_string(
    target: &mut String,
    update: Option<&String>,
    field: &'static str,
    fields: &mut Vec<String>,
) {
    if let Some(value) = update
        && target != value
    {
        target.clone_from(value);
        fields.push(field.to_string());
    }
}

fn set_option_string(
    target: &mut Option<String>,
    update: &OptionalTextUpdate,
    field: &'static str,
    fields: &mut Vec<String>,
) {
    match update {
        OptionalTextUpdate::Set(value) if target.as_ref() != Some(value) => {
            *target = Some(value.clone());
            fields.push(field.to_string());
        }
        OptionalTextUpdate::Clear if target.is_some() => {
            *target = None;
            fields.push(field.to_string());
        }
        OptionalTextUpdate::Unchanged | OptionalTextUpdate::Set(_) | OptionalTextUpdate::Clear => {}
    }
}

fn merge_metadata(
    target: &mut BTreeMap<String, Value>,
    update: &BTreeMap<String, Value>,
    fields: &mut Vec<String>,
) {
    let mut changed = false;
    for (key, value) in update {
        if value.is_null() {
            changed |= target.remove(key).is_some();
        } else if target.get(key) != Some(value) {
            target.insert(key.clone(), value.clone());
            changed = true;
        }
    }
    if changed {
        fields.push("metadata".to_string());
    }
}

fn apply_dependency_update(
    tasks: &mut [Task],
    index: usize,
    update: &TaskUpdate,
) -> Result<Vec<String>> {
    let id = tasks[index].id.clone();
    let mut fields = Vec::new();
    for blocked_id in &update.add_blocks {
        add_relation(tasks, &id, blocked_id)?;
        push_unique_field(&mut fields, "blocks");
    }
    for blocker_id in &update.add_blocked_by {
        add_relation(tasks, blocker_id, &id)?;
        push_unique_field(&mut fields, "blocked_by");
    }
    Ok(fields)
}

fn add_relation(tasks: &mut [Task], source_id: &str, target_id: &str) -> Result<()> {
    let source_index =
        find_task_index(tasks, source_id).ok_or_else(|| Error::TaskNotFound(source_id.into()))?;
    let target_index =
        find_task_index(tasks, target_id).ok_or_else(|| Error::TaskNotFound(target_id.into()))?;
    push_unique(&mut tasks[source_index].blocks, target_id);
    push_unique(&mut tasks[target_index].blocked_by, source_id);
    Ok(())
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

fn push_unique_field(fields: &mut Vec<String>, field: &str) {
    if !fields.iter().any(|existing| existing == field) {
        fields.push(field.to_string());
    }
}

fn sort_tasks(tasks: &mut [Task]) {
    tasks.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-task-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn draft(subject: &str) -> TaskDraft {
        TaskDraft {
            subject: subject.to_string(),
            description: format!("Do {subject}"),
            active_form: None,
            owner: None,
            metadata: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn create_assigns_uuid_ids_and_writes_markdown() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        let first = manager.create(draft("first")).await.unwrap();
        let second = manager.create(draft("second")).await.unwrap();

        assert_ne!(first.id, second.id);
        assert!(uuid::Uuid::parse_str(&first.id).is_ok());
        assert_eq!(first.path.as_deref(), Some("tasks/first.md"));
        assert!(dir.join("tasks").join("first.md").exists());
        assert_eq!(manager.list().await.unwrap().len(), 2);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn update_merges_metadata_and_dependencies() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        let source = manager.create(draft("blocker")).await.unwrap();
        let target = manager.create(draft("blocked")).await.unwrap();

        let mut metadata = BTreeMap::new();
        metadata.insert("kind".to_string(), Value::String("test".to_string()));
        let updated = manager
            .update(
                &target.id,
                TaskUpdate {
                    status: Some(TaskStatus::InProgress),
                    metadata: Some(metadata),
                    add_blocked_by: vec![source.id.clone()],
                    ..TaskUpdate::default()
                },
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(updated.task.status, TaskStatus::InProgress);
        assert_eq!(updated.task.blocked_by, vec![source.id.clone()]);
        assert!(updated.updated_fields.contains(&"metadata".to_string()));

        let tasks = manager.list().await.unwrap();
        let source_after = tasks.iter().find(|task| task.id == source.id).unwrap();
        assert_eq!(source_after.blocks, vec![target.id.clone()]);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn delete_removes_dependency_references() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        let first = manager.create(draft("first")).await.unwrap();
        let second = manager.create(draft("second")).await.unwrap();
        manager
            .update(
                &first.id,
                TaskUpdate {
                    add_blocks: vec![second.id.clone()],
                    ..TaskUpdate::default()
                },
            )
            .await
            .unwrap();

        assert!(manager.delete(&first.id).await.unwrap());
        let second_after = manager.get(&second.id).await.unwrap().unwrap();
        assert!(second_after.blocked_by.is_empty());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
