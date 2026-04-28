//! Model-facing cron tools.
//!
//! These tools let the model turn explicit user scheduling intent
//! into persisted cron jobs. The user-facing `/cron` command remains
//! the governance surface for listing, disabling, triggering, and
//! removing jobs.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::{Error, Result};
use super::{BaseTool, Registry, ToolOutcome};
use crate::cron::{self, CronEngine, CronJobUpdate, RunStatus, Schedule};
use crate::llm::Tool;

/// Register all model-facing cron tools.
pub fn register(registry: &mut Registry, engine: Arc<CronEngine>) {
    registry.register(Arc::new(CronCreate {
        engine: engine.clone(),
    }));
    registry.register(Arc::new(CronGet {
        engine: engine.clone(),
    }));
    registry.register(Arc::new(CronList {
        engine: engine.clone(),
    }));
    registry.register(Arc::new(CronUpdateTool {
        engine: engine.clone(),
    }));
    registry.register(Arc::new(CronDelete {
        engine: engine.clone(),
    }));
    registry.register(Arc::new(CronTrigger { engine }));
}

#[derive(Deserialize, JsonSchema)]
struct CronCreateParams {
    /// Human-readable label for the scheduled job.
    name: String,
    /// Structured schedule. Use cron for calendar rules like "daily at 9".
    schedule: ScheduleParam,
    /// Prompt that will be fed to the agent when the schedule fires.
    prompt: String,
    /// Short quote or paraphrase from the user's request authorizing this schedule.
    user_authorization: String,
    /// Whether the job should start active. Defaults to true.
    #[serde(default)]
    enabled: Option<bool>,
}

/// Create a new cron job from explicit user scheduling intent.
pub struct CronCreate {
    engine: Arc<CronEngine>,
}

#[async_trait]
impl BaseTool for CronCreate {
    fn schema(&self) -> Tool {
        Tool {
            name: "cron_create".into(),
            description: "Create a persisted scheduled prompt only when the user explicitly \
                asks for future, recurring, or delayed autonomous work. Do not create cron \
                jobs merely because a schedule would be convenient. Use user_authorization \
                to quote or paraphrase the user's scheduling request. The existing /cron \
                command remains the user-facing control surface."
                .into(),
            parameters: serde_json::to_value(schema_for!(CronCreateParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: CronCreateParams = parse_params("cron_create", args)?;
        let name = required_text("cron_create", "name", &params.name)?;
        let prompt = required_text("cron_create", "prompt", &params.prompt)?;
        let authorization = required_text(
            "cron_create",
            "user_authorization",
            &params.user_authorization,
        )?;
        let schedule = params.schedule.into_schedule("cron_create")?;

        let mut job = self
            .engine
            .add(name, schedule, prompt)
            .await
            .map_err(|err| exec("cron_create", &err))?;

        if params.enabled == Some(false) {
            self.engine
                .set_enabled(&job.id, false)
                .await
                .map_err(|err| exec("cron_create", &err))?;
            if let Some(disabled) = find_job(&self.engine, &job.id).await {
                job = disabled;
            }
        }

        Ok(json!({
            "job": job_detail(&job),
            "user_authorization": authorization,
            "message": format!("Cron job {} created: {}", job.id, job.name),
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct CronGetParams {
    /// Cron job id.
    job_id: String,
}

/// Retrieve one cron job.
pub struct CronGet {
    engine: Arc<CronEngine>,
}

#[async_trait]
impl BaseTool for CronGet {
    fn schema(&self) -> Tool {
        Tool {
            name: "cron_get".into(),
            description: "Retrieve a persisted cron job by id, including the prompt and \
                execution history."
                .into(),
            parameters: serde_json::to_value(schema_for!(CronGetParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: CronGetParams = parse_params("cron_get", args)?;
        let job = find_job(&self.engine, &params.job_id).await;
        Ok(json!({
            "job": job.as_ref().map(job_detail),
            "message": if job.is_some() { "Cron job found" } else { "Cron job not found" },
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct CronListParams {
    /// Optional enabled-state filter.
    #[serde(default)]
    enabled: Option<bool>,
    /// Include full prompts in each row. Defaults to false for compactness.
    #[serde(default)]
    include_prompt: Option<bool>,
}

/// List persisted cron jobs.
pub struct CronList {
    engine: Arc<CronEngine>,
}

#[async_trait]
impl BaseTool for CronList {
    fn schema(&self) -> Tool {
        Tool {
            name: "cron_list".into(),
            description: "List persisted cron jobs. Use this before creating a new schedule \
                to avoid duplicates, and when the user asks what automated work is active."
                .into(),
            parameters: serde_json::to_value(schema_for!(CronListParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: CronListParams = parse_params("cron_list", args)?;
        let status = self.engine.status().await;
        let include_prompt = params.include_prompt.unwrap_or(false);
        let jobs: Vec<Value> = status
            .jobs
            .iter()
            .filter(|job| params.enabled.is_none_or(|enabled| job.enabled == enabled))
            .map(|job| job_summary(job, include_prompt))
            .collect();
        Ok(json!({
            "enabled": status.enabled,
            "jobs": jobs,
            "count": jobs.len(),
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct CronUpdateParams {
    /// Cron job id.
    job_id: String,
    /// Replacement human-readable label.
    #[serde(default)]
    name: Option<String>,
    /// Replacement structured schedule.
    #[serde(default)]
    schedule: Option<ScheduleParam>,
    /// Replacement prompt fed to the agent when the job fires.
    #[serde(default)]
    prompt: Option<String>,
    /// Replacement enabled state.
    #[serde(default)]
    enabled: Option<bool>,
}

/// Update a cron job definition.
pub struct CronUpdateTool {
    engine: Arc<CronEngine>,
}

#[async_trait]
impl BaseTool for CronUpdateTool {
    fn schema(&self) -> Tool {
        Tool {
            name: "cron_update".into(),
            description: "Update a persisted cron job's definition or enabled state. Use this \
                when the user asks to change an existing automated schedule. If a schedule \
                update would create new autonomous behavior, the user must have clearly \
                requested that change."
                .into(),
            parameters: serde_json::to_value(schema_for!(CronUpdateParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: CronUpdateParams = parse_params("cron_update", args)?;
        let name = params
            .name
            .map(|value| required_text("cron_update", "name", &value))
            .transpose()?;
        let prompt = params
            .prompt
            .map(|value| required_text("cron_update", "prompt", &value))
            .transpose()?;
        let schedule = params
            .schedule
            .map(|value| value.into_schedule("cron_update"))
            .transpose()?;

        let outcome = self
            .engine
            .update(
                &params.job_id,
                CronJobUpdate {
                    name,
                    schedule,
                    prompt,
                    enabled: params.enabled,
                },
            )
            .await
            .map_err(|err| exec("cron_update", &err))?;
        let Some(outcome) = outcome else {
            return Ok(json!({
                "success": false,
                "job_id": params.job_id,
                "message": "Cron job not found",
            })
            .into());
        };

        Ok(json!({
            "success": true,
            "job_id": outcome.job.id,
            "updated_fields": outcome.updated_fields,
            "job": job_detail(&outcome.job),
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct CronDeleteParams {
    /// Cron job id.
    job_id: String,
}

/// Delete a cron job.
pub struct CronDelete {
    engine: Arc<CronEngine>,
}

#[async_trait]
impl BaseTool for CronDelete {
    fn schema(&self) -> Tool {
        Tool {
            name: "cron_delete".into(),
            description: "Delete a persisted cron job when the user asks to remove an \
                automated schedule."
                .into(),
            parameters: serde_json::to_value(schema_for!(CronDeleteParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: CronDeleteParams = parse_params("cron_delete", args)?;
        match self.engine.remove(&params.job_id).await {
            Ok(()) => Ok(json!({
                "success": true,
                "job_id": params.job_id,
                "message": "Cron job deleted",
            })
            .into()),
            Err(cron::Error::JobNotFound(_)) => Ok(json!({
                "success": false,
                "job_id": params.job_id,
                "message": "Cron job not found",
            })
            .into()),
            Err(err) => Err(exec("cron_delete", &err)),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct CronTriggerParams {
    /// Cron job id.
    job_id: String,
}

/// Trigger a cron job on the next scheduler pass.
pub struct CronTrigger {
    engine: Arc<CronEngine>,
}

#[async_trait]
impl BaseTool for CronTrigger {
    fn schema(&self) -> Tool {
        Tool {
            name: "cron_trigger".into(),
            description: "Request an immediate run of an enabled cron job. Disabled jobs are \
                not triggered; enable the job first if the user asks for that."
                .into(),
            parameters: serde_json::to_value(schema_for!(CronTriggerParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: CronTriggerParams = parse_params("cron_trigger", args)?;
        let Some(job) = find_job(&self.engine, &params.job_id).await else {
            return Ok(json!({
                "success": false,
                "job_id": params.job_id,
                "message": "Cron job not found",
            })
            .into());
        };
        if !job.enabled {
            return Ok(json!({
                "success": false,
                "job_id": params.job_id,
                "message": "Cron job is disabled",
            })
            .into());
        }

        self.engine
            .trigger(&params.job_id)
            .await
            .map_err(|err| exec("cron_trigger", &err))?;
        Ok(json!({
            "success": true,
            "job_id": params.job_id,
            "message": "Cron job trigger requested",
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

async fn find_job(engine: &CronEngine, job_id: &str) -> Option<cron::CronJob> {
    engine
        .status()
        .await
        .jobs
        .into_iter()
        .find(|job| job.id == job_id)
}

fn job_summary(job: &cron::CronJob, include_prompt: bool) -> Value {
    let mut value = json!({
        "id": &job.id,
        "name": &job.name,
        "enabled": job.enabled,
        "schedule": job.schedule.describe(),
        "next_run_at": job.state.next_run_at.map(|time| time.to_rfc3339()),
        "last_run_at": job.state.last_run_at.map(|time| time.to_rfc3339()),
        "last_status": job.state.last_status.map(status_name),
        "consecutive_errors": job.state.consecutive_errors,
    });
    if include_prompt {
        value["prompt"] = json!(&job.prompt);
    }
    value
}

fn job_detail(job: &cron::CronJob) -> Value {
    json!({
        "id": &job.id,
        "name": &job.name,
        "enabled": job.enabled,
        "schedule": &job.schedule,
        "schedule_description": job.schedule.describe(),
        "prompt": &job.prompt,
        "next_run_at": job.state.next_run_at.map(|time| time.to_rfc3339()),
        "last_run_at": job.state.last_run_at.map(|time| time.to_rfc3339()),
        "last_status": job.state.last_status.map(status_name),
        "last_error": &job.state.last_error,
        "consecutive_errors": job.state.consecutive_errors,
        "created_at": job.created_at.to_rfc3339(),
        "updated_at": job.updated_at.to_rfc3339(),
    })
}

fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Skipped => "skipped",
    }
}

fn required_text(tool: &'static str, field: &'static str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(exec(tool, &format_args!("{field} must not be empty")));
    }
    Ok(trimmed.to_string())
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
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::*;
    use crate::cron::CronConfig;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-cron-tool-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    async fn engine(dir: &Path) -> Arc<CronEngine> {
        let cfg = CronConfig { enabled: true };
        let (engine, _rx) = CronEngine::new(&cfg, dir).await.unwrap();
        Arc::new(engine)
    }

    #[tokio::test]
    async fn cron_tools_create_update_list_and_delete() {
        let dir = tempdir();
        let engine = engine(&dir).await;
        let create = CronCreate {
            engine: engine.clone(),
        };
        let update = CronUpdateTool {
            engine: engine.clone(),
        };
        let list = CronList {
            engine: engine.clone(),
        };
        let delete = CronDelete {
            engine: engine.clone(),
        };

        let result = create
            .call(json!({
                "name": "Daily issue check",
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "prompt": "Check open issues and summarize anything new.",
                "user_authorization": "daily check open issues"
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("cron_create should return plain result");
        };
        let job_id = value["job"]["id"].as_str().unwrap().to_string();

        update
            .call(json!({
                "job_id": job_id,
                "name": "Morning issue check",
                "enabled": false
            }))
            .await
            .unwrap();
        let result = list
            .call(json!({
                "enabled": false,
                "include_prompt": true
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("cron_list should return plain result");
        };
        assert_eq!(value["count"], 1);
        assert_eq!(value["jobs"][0]["name"], "Morning issue check");
        assert_eq!(value["jobs"][0]["enabled"], false);
        assert!(
            value["jobs"][0]["prompt"]
                .as_str()
                .unwrap()
                .contains("issues")
        );

        delete.call(json!({ "job_id": job_id })).await.unwrap();
        assert!(engine.status().await.jobs.is_empty());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
