//! Model-facing task tools.
//!
//! These tools are intentionally not slash commands. They give the
//! model a durable project-local task list it can maintain while it
//! works: create tasks, mark progress, assign owners, and express
//! dependencies. Users observe the result through normal replies and
//! future UI surfaces rather than hand-editing the list.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::{Error, Result};
use super::{BaseTool, Registry, ToolOutcome};
use crate::llm::Tool;
use crate::task::{self, OptionalTextUpdate, TaskDraft, TaskStatus, TaskUpdate};

/// Register all model-facing task tools.
pub fn register(registry: &mut Registry, tasks: Arc<task::Manager>) {
    registry.register(Arc::new(TaskCreate {
        tasks: tasks.clone(),
    }));
    registry.register(Arc::new(TaskGet {
        tasks: tasks.clone(),
    }));
    registry.register(Arc::new(TaskList {
        tasks: tasks.clone(),
    }));
    registry.register(Arc::new(TaskUpdateTool { tasks }));
}

#[derive(Deserialize, JsonSchema)]
struct TaskCreateParams {
    /// Brief actionable title in imperative form.
    subject: String,
    /// Full description of what needs to be done.
    description: String,
    /// Present continuous form shown while in progress, e.g. "Running tests".
    #[serde(default)]
    active_form: Option<String>,
    /// Optional owner / agent id.
    #[serde(default)]
    owner: Option<String>,
    /// Optional free-form metadata.
    #[serde(default)]
    metadata: Option<BTreeMap<String, Value>>,
}

/// Create a new task.
pub struct TaskCreate {
    tasks: Arc<task::Manager>,
}

#[async_trait]
impl BaseTool for TaskCreate {
    fn schema(&self) -> Tool {
        Tool {
            name: "task_create".into(),
            description: "Create a project-local task for complex work. Use proactively \
                for multi-step requests, after receiving several requirements, or when \
                discovering follow-up work. Created tasks start as pending. This is \
                model-facing state, not a user slash command."
                .into(),
            parameters: serde_json::to_value(schema_for!(TaskCreateParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TaskCreateParams = parse_params("task_create", args)?;
        let task = self
            .tasks
            .create(TaskDraft {
                subject: params.subject,
                description: params.description,
                active_form: params.active_form,
                owner: params.owner,
                metadata: params.metadata.unwrap_or_default(),
            })
            .await
            .map_err(|err| exec("task_create", &err))?;
        Ok(json!({
            "task": task_summary(&task, &[]),
            "message": format!("Task #{} created: {}", task.id, task.subject),
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct TaskGetParams {
    /// Task id to retrieve.
    task_id: String,
}

/// Retrieve one task.
pub struct TaskGet {
    tasks: Arc<task::Manager>,
}

#[async_trait]
impl BaseTool for TaskGet {
    fn schema(&self) -> Tool {
        Tool {
            name: "task_get".into(),
            description: "Retrieve a task by id before updating it or when more detail \
                is needed than task_list provides."
                .into(),
            parameters: serde_json::to_value(schema_for!(TaskGetParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TaskGetParams = parse_params("task_get", args)?;
        let task = self
            .tasks
            .get(&params.task_id)
            .await
            .map_err(|err| exec("task_get", &err))?;
        let Some(task) = task else {
            return Ok(json!({ "task": null, "message": "Task not found" }).into());
        };
        let tasks = self
            .tasks
            .list()
            .await
            .map_err(|err| exec("task_get", &err))?;
        Ok(json!({ "task": task_detail(&task, &tasks) }).into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct TaskListParams {
    /// Optional status filter.
    #[serde(default)]
    status: Option<TaskStatusParam>,
    /// Optional owner filter.
    #[serde(default)]
    owner: Option<String>,
    /// Include completed tasks. Defaults to true so the model can see
    /// the full board unless it explicitly filters.
    #[serde(default)]
    include_completed: Option<bool>,
}

/// List tasks.
pub struct TaskList {
    tasks: Arc<task::Manager>,
}

#[async_trait]
impl BaseTool for TaskList {
    fn schema(&self) -> Tool {
        Tool {
            name: "task_list".into(),
            description: "List project-local tasks. Shows status, owner, and unresolved \
                blockers. Call this before creating duplicate tasks, when resuming work, \
                or after completing a task to find what remains."
                .into(),
            parameters: serde_json::to_value(schema_for!(TaskListParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TaskListParams = parse_params("task_list", args)?;
        let tasks = self
            .tasks
            .list()
            .await
            .map_err(|err| exec("task_list", &err))?;
        let filtered: Vec<Value> = tasks
            .iter()
            .filter(|task| should_include_task(task, &tasks, &params))
            .map(|task| task_summary(task, &tasks))
            .collect();
        Ok(json!({
            "tasks": filtered,
            "count": filtered.len(),
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct TaskUpdateParams {
    /// Task id to update.
    task_id: String,
    /// Replacement title.
    #[serde(default)]
    subject: Option<String>,
    /// Replacement description.
    #[serde(default)]
    description: Option<String>,
    /// Replacement present-continuous form.
    #[serde(default)]
    active_form: Option<String>,
    /// Clear `active_form` when true.
    #[serde(default)]
    clear_active_form: Option<bool>,
    /// Replacement owner / agent id.
    #[serde(default)]
    owner: Option<String>,
    /// Clear owner when true.
    #[serde(default)]
    clear_owner: Option<bool>,
    /// New status. Use deleted to remove the task entirely.
    #[serde(default)]
    status: Option<TaskUpdateStatus>,
    /// Task ids that this task blocks.
    #[serde(default)]
    add_blocks: Vec<String>,
    /// Task ids that block this task.
    #[serde(default)]
    add_blocked_by: Vec<String>,
    /// Metadata keys to merge. Set a key to null to delete it.
    #[serde(default)]
    metadata: Option<BTreeMap<String, Value>>,
}

/// Update or delete a task.
pub struct TaskUpdateTool {
    tasks: Arc<task::Manager>,
}

#[async_trait]
impl BaseTool for TaskUpdateTool {
    fn schema(&self) -> Tool {
        Tool {
            name: "task_update".into(),
            description: "Update a task's status, owner, details, metadata, or \
                dependencies. Mark a task in_progress before starting it and completed \
                only after the work is fully done. Use status=deleted only for tasks \
                created in error or no longer relevant."
                .into(),
            parameters: serde_json::to_value(schema_for!(TaskUpdateParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TaskUpdateParams = parse_params("task_update", args)?;
        if params.status == Some(TaskUpdateStatus::Deleted) {
            return self.delete_task(&params.task_id).await;
        }

        let status = params.status.map(TaskStatus::from);
        let active_form = optional_field(params.active_form, params.clear_active_form);
        let owner = optional_field(params.owner, params.clear_owner);
        let outcome = self
            .tasks
            .update(
                &params.task_id,
                TaskUpdate {
                    subject: params.subject,
                    description: params.description,
                    active_form,
                    owner,
                    status,
                    metadata: params.metadata,
                    add_blocks: params.add_blocks,
                    add_blocked_by: params.add_blocked_by,
                },
            )
            .await
            .map_err(|err| exec("task_update", &err))?;
        let Some(outcome) = outcome else {
            return Ok(json!({
                "success": false,
                "task_id": params.task_id,
                "message": "Task not found",
            })
            .into());
        };
        let tasks = self
            .tasks
            .list()
            .await
            .map_err(|err| exec("task_update", &err))?;
        Ok(json!({
            "success": true,
            "task_id": outcome.task.id,
            "updated_fields": outcome.updated_fields,
            "task": task_summary(&outcome.task, &tasks),
        })
        .into())
    }
}

impl TaskUpdateTool {
    async fn delete_task(&self, task_id: &str) -> Result<ToolOutcome> {
        let deleted = self
            .tasks
            .delete(task_id)
            .await
            .map_err(|err| exec("task_update", &err))?;
        Ok(json!({
            "success": deleted,
            "task_id": task_id,
            "updated_fields": if deleted { vec!["deleted"] } else { Vec::<&str>::new() },
            "message": if deleted { "Task deleted" } else { "Task not found" },
        })
        .into())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
enum TaskStatusParam {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
enum TaskUpdateStatus {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

impl From<TaskUpdateStatus> for TaskStatus {
    fn from(value: TaskUpdateStatus) -> Self {
        match value {
            TaskUpdateStatus::Pending => Self::Pending,
            TaskUpdateStatus::InProgress => Self::InProgress,
            TaskUpdateStatus::Completed => Self::Completed,
            TaskUpdateStatus::Deleted => unreachable!("deleted is handled before conversion"),
        }
    }
}

fn should_include_task(
    task: &task::Task,
    all_tasks: &[task::Task],
    params: &TaskListParams,
) -> bool {
    if params.include_completed == Some(false) && task.status == TaskStatus::Completed {
        return false;
    }
    if let Some(owner) = params.owner.as_deref()
        && task.owner.as_deref() != Some(owner)
    {
        return false;
    }
    match params.status {
        Some(TaskStatusParam::Pending) => task.status == TaskStatus::Pending,
        Some(TaskStatusParam::InProgress) => task.status == TaskStatus::InProgress,
        Some(TaskStatusParam::Completed) => task.status == TaskStatus::Completed,
        Some(TaskStatusParam::Blocked) => !task::unresolved_blockers(task, all_tasks).is_empty(),
        None => true,
    }
}

fn task_summary(task: &task::Task, all_tasks: &[task::Task]) -> Value {
    let unresolved = task::unresolved_blockers(task, all_tasks);
    json!({
        "id": &task.id,
        "path": &task.path,
        "subject": &task.subject,
        "status": status_name(task.status),
        "owner": &task.owner,
        "blocked": !unresolved.is_empty(),
        "blocked_by": unresolved,
    })
}

fn task_detail(task: &task::Task, all_tasks: &[task::Task]) -> Value {
    let unresolved = task::unresolved_blockers(task, all_tasks);
    json!({
        "id": &task.id,
        "path": &task.path,
        "subject": &task.subject,
        "description": &task.description,
        "active_form": &task.active_form,
        "owner": &task.owner,
        "status": status_name(task.status),
        "blocks": &task.blocks,
        "blocked_by": &task.blocked_by,
        "unresolved_blockers": unresolved,
        "metadata": &task.metadata,
        "created_at": task.created_at.to_rfc3339(),
        "updated_at": task.updated_at.to_rfc3339(),
    })
}

fn optional_field(value: Option<String>, clear: Option<bool>) -> OptionalTextUpdate {
    if clear.unwrap_or(false) {
        OptionalTextUpdate::Clear
    } else {
        value.map_or(OptionalTextUpdate::Unchanged, OptionalTextUpdate::Set)
    }
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(tool: &'static str, args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|source| Error::InvalidArguments {
        tool: tool.to_string(),
        source,
    })
}

fn exec(tool: &'static str, message: &impl std::fmt::Display) -> Error {
    Error::Execution {
        tool: tool.to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-task-tool-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test]
    async fn task_tools_create_update_and_list() {
        let dir = tempdir();
        let manager = Arc::new(task::Manager::new(&dir));
        let create = TaskCreate {
            tasks: manager.clone(),
        };
        let update = TaskUpdateTool {
            tasks: manager.clone(),
        };
        let list = TaskList {
            tasks: manager.clone(),
        };

        let result = create
            .call(json!({
                "subject": "Run tests",
                "description": "Run the Rust test suite",
                "active_form": "Running tests"
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("task_create should return plain result");
        };
        let task_id = value["task"]["id"].as_str().unwrap().to_string();
        update
            .call(json!({
                "task_id": task_id,
                "status": "in_progress",
                "owner": "main"
            }))
            .await
            .unwrap();
        let result = list
            .call(json!({ "include_completed": false }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("task_list should return plain result");
        };
        assert_eq!(value["count"], 1);
        assert_eq!(value["tasks"][0]["status"], "in_progress");

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
