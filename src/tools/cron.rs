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
use crate::cron::{self, CronEngine, RunStatus, Schedule};
use crate::llm::Tool;

/// Register all model-facing cron tools.
pub fn register(registry: &mut Registry, engine: Arc<CronEngine>) {
    registry.register(Arc::new(CronCreate {
        engine: engine.clone(),
    }));
    registry.register(Arc::new(CronList {
        engine: engine.clone(),
    }));
    registry.register(Arc::new(CronDelete { engine }));
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

        let job = self
            .engine
            .add(name, schedule, prompt)
            .await
            .map_err(|err| exec("cron_create", &err))?;

        Ok(json!({
            "job": job_detail(&job),
            "user_authorization": authorization,
            "message": format!("Cron job {} created: {}", job.id, job.name),
        })
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct CronListParams {}

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
        let _: CronListParams = parse_params("cron_list", args)?;
        let status = self.engine.status().await;
        let jobs: Vec<Value> = status.jobs.iter().map(job_summary).collect();
        Ok(json!({
            "enabled": status.enabled,
            "jobs": jobs,
            "count": jobs.len(),
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

fn job_summary(job: &cron::CronJob) -> Value {
    json!({
        "id": &job.id,
        "name": &job.name,
        "enabled": job.enabled,
        "schedule": job.schedule.describe(),
        "prompt": &job.prompt,
        "next_run_at": job.state.next_run_at.map(|time| time.to_rfc3339()),
        "last_run_at": job.state.last_run_at.map(|time| time.to_rfc3339()),
        "last_status": job.state.last_status.map(status_name),
        "consecutive_errors": job.state.consecutive_errors,
    })
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
    async fn cron_tools_create_list_and_delete() {
        let dir = tempdir();
        let engine = engine(&dir).await;
        let create = CronCreate {
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

        let result = list.call(json!({})).await.unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("cron_list should return plain result");
        };
        assert_eq!(value["count"], 1);
        assert_eq!(value["jobs"][0]["name"], "Daily issue check");
        assert_eq!(value["jobs"][0]["enabled"], true);
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
