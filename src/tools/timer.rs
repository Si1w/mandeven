//! Model-facing timer tools.
//!
//! Timers are validated Markdown state. They bind a task id to a
//! schedule and make cron-style work composable as `task + timer`
//! rather than as a separate high-level cron instruction.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::{Error, Result};
use super::{BaseTool, Registry, ToolOutcome};
use crate::llm::Tool;
use crate::timer::{self, Schedule, TimerDraft, TimerUpdate};

/// Register all model-facing timer tools.
pub fn register(registry: &mut Registry, timers: Arc<timer::Manager>) {
    registry.register(Arc::new(TimerCreate {
        timers: timers.clone(),
    }));
    registry.register(Arc::new(TimerList {
        timers: timers.clone(),
    }));
    registry.register(Arc::new(TimerUpdateTool {
        timers: timers.clone(),
    }));
    registry.register(Arc::new(TimerDelete {
        timers: timers.clone(),
    }));
    registry.register(Arc::new(TimerFireNow { timers }));
}

#[derive(Deserialize, JsonSchema)]
struct TimerCreateParams {
    /// Human-readable timer title.
    title: String,
    /// Existing task id this timer should fire.
    task_id: String,
    /// Structured schedule.
    schedule: ScheduleParam,
}

/// Create a timer for an existing task.
pub struct TimerCreate {
    timers: Arc<timer::Manager>,
}

#[async_trait]
impl BaseTool for TimerCreate {
    fn schema(&self) -> Tool {
        Tool {
            name: "timer_create".into(),
            description: "Create validated timer Markdown state for an existing task. \
                Use this for delayed, recurring, or calendar-based work after task_create \
                has produced the task. This replaces model-facing cron creation."
                .into(),
            parameters: serde_json::to_value(schema_for!(TimerCreateParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TimerCreateParams = parse_params("timer_create", args)?;
        let timer = self
            .timers
            .create(TimerDraft {
                title: params.title,
                task_id: params.task_id,
                schedule: params.schedule.into_schedule("timer_create")?,
            })
            .await
            .map_err(|err| exec("timer_create", &err))?;
        Ok(state_observation("timer", &timer, "Timer created").into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct TimerListParams {
    /// Optional task id filter.
    #[serde(default)]
    task_id: Option<String>,
    /// Include disabled timers. Defaults to true.
    #[serde(default)]
    include_disabled: Option<bool>,
}

/// List timers.
pub struct TimerList {
    timers: Arc<timer::Manager>,
}

#[async_trait]
impl BaseTool for TimerList {
    fn schema(&self) -> Tool {
        Tool {
            name: "timer_list".into(),
            description: "List timer Markdown state. Use this before creating a schedule \
                to avoid duplicates, and when the user asks what automated work is active."
                .into(),
            parameters: serde_json::to_value(schema_for!(TimerListParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TimerListParams = parse_params("timer_list", args)?;
        let timers = self
            .timers
            .list()
            .await
            .map_err(|err| exec("timer_list", &err))?;
        let filtered: Vec<Value> = timers
            .iter()
            .filter(|timer| should_include_timer(timer, &params))
            .map(timer_summary)
            .collect();
        Ok(json!({
            "ok": true,
            "observation_type": "state",
            "object": "timer",
            "validated": true,
            "diagnostics": [],
            "timers": filtered,
            "count": filtered.len(),
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct TimerUpdateParams {
    /// Timer id to update.
    timer_id: String,
    /// Replacement title.
    #[serde(default)]
    title: Option<String>,
    /// Replacement task id.
    #[serde(default)]
    task_id: Option<String>,
    /// Replacement enabled flag.
    #[serde(default)]
    enabled: Option<bool>,
    /// Replacement schedule.
    #[serde(default)]
    schedule: Option<ScheduleParam>,
}

/// Update timer state.
pub struct TimerUpdateTool {
    timers: Arc<timer::Manager>,
}

#[async_trait]
impl BaseTool for TimerUpdateTool {
    fn schema(&self) -> Tool {
        Tool {
            name: "timer_update".into(),
            description: "Update a timer's title, referenced task, enabled flag, or \
                schedule. This edits validated Markdown state and recomputes next_fire_at \
                when the schedule or enabled flag changes."
                .into(),
            parameters: serde_json::to_value(schema_for!(TimerUpdateParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TimerUpdateParams = parse_params("timer_update", args)?;
        let schedule = params
            .schedule
            .map(|schedule| schedule.into_schedule("timer_update"))
            .transpose()?;
        let outcome = self
            .timers
            .update(
                &params.timer_id,
                TimerUpdate {
                    title: params.title,
                    task_id: params.task_id,
                    enabled: params.enabled,
                    schedule,
                },
            )
            .await
            .map_err(|err| exec("timer_update", &err))?;
        let Some(outcome) = outcome else {
            return Ok(json!({
                "ok": false,
                "observation_type": "state",
                "object": "timer",
                "id": params.timer_id,
                "validated": false,
                "diagnostics": ["Timer not found"],
            })
            .into());
        };
        let mut observation = state_observation("timer", &outcome.timer, "Timer updated");
        observation["updated_fields"] = json!(outcome.updated_fields);
        Ok(observation.into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct TimerDeleteParams {
    /// Timer id to delete.
    timer_id: String,
}

/// Delete timer state.
pub struct TimerDelete {
    timers: Arc<timer::Manager>,
}

#[async_trait]
impl BaseTool for TimerDelete {
    fn schema(&self) -> Tool {
        Tool {
            name: "timer_delete".into(),
            description: "Delete timer Markdown state when a schedule is no longer needed.".into(),
            parameters: serde_json::to_value(schema_for!(TimerDeleteParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TimerDeleteParams = parse_params("timer_delete", args)?;
        let deleted = self
            .timers
            .delete(&params.timer_id)
            .await
            .map_err(|err| exec("timer_delete", &err))?;
        Ok(json!({
            "ok": deleted,
            "observation_type": "state",
            "object": "timer",
            "id": params.timer_id,
            "validated": deleted,
            "diagnostics": if deleted { Vec::<&str>::new() } else { vec!["Timer not found"] },
            "message": if deleted { "Timer deleted" } else { "Timer not found" },
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct TimerFireNowParams {
    /// Timer id to mark fired.
    timer_id: String,
}

/// Mark a timer as fired now and expose the task to run.
pub struct TimerFireNow {
    timers: Arc<timer::Manager>,
}

#[async_trait]
impl BaseTool for TimerFireNow {
    fn schema(&self) -> Tool {
        Tool {
            name: "timer_fire_now".into(),
            description: "Validate and mark a timer as fired now, then return the \
                referenced task as the next execution target. This state primitive does \
                not run the agent loop itself."
                .into(),
            parameters: serde_json::to_value(schema_for!(TimerFireNowParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: TimerFireNowParams = parse_params("timer_fire_now", args)?;
        let outcome = self
            .timers
            .fire_now(&params.timer_id)
            .await
            .map_err(|err| exec("timer_fire_now", &err))?;
        let Some(outcome) = outcome else {
            return Ok(json!({
                "ok": false,
                "observation_type": "state",
                "object": "timer_fire",
                "id": params.timer_id,
                "validated": false,
                "diagnostics": ["Timer not found"],
            })
            .into());
        };
        Ok(json!({
            "ok": true,
            "observation_type": "state",
            "object": "timer_fire",
            "id": outcome.timer.id,
            "path": outcome.timer.path,
            "validated": true,
            "diagnostics": [],
            "spec": timer_spec(&outcome.timer),
            "task": {
                "id": outcome.task.id,
                "path": outcome.task.path,
                "subject": outcome.task.subject,
                "description": outcome.task.description,
            },
            "message": "Timer marked fired; run the returned task through the task runner",
        })
        .into())
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScheduleParam {
    /// Fire once at an absolute RFC3339 timestamp.
    At {
        /// RFC3339 timestamp, e.g. 2026-04-28T09:00:00Z.
        at: String,
    },
    /// Fire repeatedly at a fixed interval.
    Every {
        /// Seconds between runs. Must be greater than zero.
        interval_secs: i64,
        /// Optional RFC3339 anchor. Defaults to creation time.
        #[serde(default)]
        anchor: Option<String>,
    },
    /// Fire on a cron expression.
    Cron {
        /// Vixie-style 5-field expression, or 6/7 fields for advanced use.
        expr: String,
    },
}

impl ScheduleParam {
    fn into_schedule(self, tool: &'static str) -> Result<Schedule> {
        match self {
            Self::At { at } => Ok(Schedule::at(parse_utc(tool, "at", &at)?)),
            Self::Every {
                interval_secs,
                anchor,
            } => {
                let anchor = anchor
                    .as_deref()
                    .map(|value| parse_utc(tool, "anchor", value))
                    .transpose()?
                    .unwrap_or_else(Utc::now);
                Schedule::every(Duration::seconds(interval_secs), anchor)
                    .map_err(|err| exec(tool, &err))
            }
            Self::Cron { expr } => Schedule::cron(&expr).map_err(|err| exec(tool, &err)),
        }
    }
}

fn should_include_timer(timer: &timer::Timer, params: &TimerListParams) -> bool {
    if params.include_disabled == Some(false) && !timer.enabled {
        return false;
    }
    if let Some(task_id) = params.task_id.as_deref()
        && timer.task_id != task_id
    {
        return false;
    }
    true
}

fn state_observation(object: &'static str, timer: &timer::Timer, message: &'static str) -> Value {
    json!({
        "ok": true,
        "observation_type": "state",
        "object": object,
        "id": &timer.id,
        "path": &timer.path,
        "validated": true,
        "diagnostics": [],
        "spec": timer_spec(timer),
        "message": message,
    })
}

fn timer_summary(timer: &timer::Timer) -> Value {
    json!({
        "id": &timer.id,
        "path": &timer.path,
        "title": &timer.title,
        "task_id": &timer.task_id,
        "enabled": timer.enabled,
        "schedule": timer.schedule.describe(),
        "next_fire_at": timer.next_fire_at.map(|time| time.to_rfc3339()),
        "last_fire_at": timer.last_fire_at.map(|time| time.to_rfc3339()),
    })
}

fn timer_spec(timer: &timer::Timer) -> Value {
    json!({
        "title": &timer.title,
        "task_id": &timer.task_id,
        "enabled": timer.enabled,
        "schedule": &timer.schedule,
        "schedule_description": timer.schedule.describe(),
        "next_fire_at": timer.next_fire_at.map(|time| time.to_rfc3339()),
        "last_fire_at": timer.last_fire_at.map(|time| time.to_rfc3339()),
        "created_at": timer.created_at.to_rfc3339(),
        "updated_at": timer.updated_at.to_rfc3339(),
    })
}

fn parse_utc(tool: &'static str, field: &'static str, value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| exec(tool, &format_args!("invalid {field} timestamp: {err}")))
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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::task;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-timer-tool-test-{}", uuid::Uuid::now_v7()));
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
    async fn timer_tools_create_list_update_fire_and_delete() {
        let dir = tempdir();
        let tasks = task::Manager::new(&dir);
        let task = tasks.create(task_draft("Run tests")).await.unwrap();
        let manager = Arc::new(timer::Manager::new(&dir));
        let create = TimerCreate {
            timers: manager.clone(),
        };
        let list = TimerList {
            timers: manager.clone(),
        };
        let update = TimerUpdateTool {
            timers: manager.clone(),
        };
        let fire = TimerFireNow {
            timers: manager.clone(),
        };
        let delete = TimerDelete {
            timers: manager.clone(),
        };

        let result = create
            .call(json!({
                "title": "Daily test run",
                "task_id": task.id,
                "schedule": { "kind": "cron", "expr": "0 9 * * *" }
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("timer_create should return plain result");
        };
        let timer_id = value["id"].as_str().unwrap().to_string();
        assert_eq!(value["path"], "timers/daily-test-run.md");

        let result = list
            .call(json!({ "include_disabled": false }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("timer_list should return plain result");
        };
        assert_eq!(value["count"], 1);

        update
            .call(json!({
                "timer_id": timer_id,
                "enabled": false
            }))
            .await
            .unwrap();
        let result = fire.call(json!({ "timer_id": timer_id })).await.unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("timer_fire_now should return plain result");
        };
        assert_eq!(value["task"]["subject"], "Run tests");

        delete.call(json!({ "timer_id": timer_id })).await.unwrap();
        assert!(manager.list().await.unwrap().is_empty());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
