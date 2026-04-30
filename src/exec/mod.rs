//! Machine-readable execution history.
//!
//! Executions are runtime event streams, so their canonical store is
//! JSONL under `<project_bucket>/execution/<exec_id>.jsonl`. This
//! module only records the durable event stream; user-visible
//! deliverables should remain Markdown files elsewhere.

pub mod error;

pub use error::{Error, Result};

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::bus::{ChannelID, SessionID};

/// Subdirectory under a project bucket holding execution JSONL logs.
pub const EXEC_SUBDIR: &str = "execution";

/// Execution history manager.
#[derive(Debug, Clone)]
pub struct Manager {
    dir: PathBuf,
}

/// Stable id for one execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecId(pub Uuid);

impl ExecId {
    /// Parse an execution id from its UUID string form.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidExecId`] when `raw` is not a UUID.
    pub fn parse(raw: &str) -> Result<Self> {
        Uuid::parse_str(raw)
            .map(Self)
            .map_err(|_| Error::InvalidExecId(raw.to_string()))
    }
}

impl std::fmt::Display for ExecId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Input for starting an execution log.
#[derive(Clone, Debug)]
pub struct ExecStart {
    /// Task being executed.
    pub task_id: String,
    /// Human-readable task subject.
    pub task_subject: String,
    /// Timer that triggered the execution, when scheduled.
    pub timer_id: Option<String>,
    /// Human-readable timer title.
    pub timer_title: Option<String>,
    /// Session receiving the execution output.
    pub session: SessionID,
    /// Channel receiving the execution output.
    pub channel: ChannelID,
}

/// What caused an execution to start.
#[derive(Clone, Debug)]
pub enum ExecTrigger {
    /// Direct task execution.
    TaskRun,
    /// Timer scheduler fired.
    Timer {
        /// Timer that triggered the execution.
        timer_id: String,
        /// Human-readable timer title.
        timer_title: String,
    },
}

impl ExecTrigger {
    fn timer_id(&self) -> Option<String> {
        match self {
            Self::TaskRun => None,
            Self::Timer { timer_id, .. } => Some(timer_id.clone()),
        }
    }

    fn timer_title(&self) -> Option<String> {
        match self {
            Self::TaskRun => None,
            Self::Timer { timer_title, .. } => Some(timer_title.clone()),
        }
    }
}

/// Validated task execution input.
#[derive(Clone, Debug)]
pub struct TaskExecution {
    /// Task being executed.
    pub task_id: String,
    /// Human-readable task subject.
    pub task_subject: String,
    /// User-message text fed into the agent.
    pub prompt: String,
    /// Trigger that started the execution.
    pub trigger: ExecTrigger,
}

impl TaskExecution {
    /// Build the history start record for this execution.
    #[must_use]
    pub fn start(&self, session: SessionID, channel: ChannelID) -> ExecStart {
        ExecStart {
            task_id: self.task_id.clone(),
            task_subject: self.task_subject.clone(),
            timer_id: self.trigger.timer_id(),
            timer_title: self.trigger.timer_title(),
            session,
            channel,
        }
    }
}

/// Terminal execution status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStatus {
    /// Execution completed without an agent-loop error.
    Succeeded,
    /// Execution failed with an agent-loop error.
    Failed,
    /// Execution was skipped before it began.
    Skipped,
}

/// One line in an execution JSONL file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecEvent {
    /// Execution was accepted by the runtime.
    ExecutionStarted {
        /// Execution id.
        exec_id: String,
        /// Task being executed.
        task_id: String,
        /// Human-readable task subject.
        task_subject: String,
        /// Timer that triggered the execution, when scheduled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timer_id: Option<String>,
        /// Human-readable timer title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timer_title: Option<String>,
        /// Session receiving the execution output.
        session_id: String,
        /// Channel receiving the execution output.
        channel_id: String,
        /// Wall-clock timestamp.
        at: DateTime<Utc>,
    },
    /// Final assistant-facing output from the execution.
    FinalOutput {
        /// Final answer text. Empty means the model ended without
        /// assistant text.
        content: String,
        /// Wall-clock timestamp.
        at: DateTime<Utc>,
    },
    /// Tool call emitted by the model.
    ToolCall {
        /// Tool call id from the provider.
        tool_call_id: String,
        /// Tool name.
        name: String,
        /// Parsed tool arguments, or `null` if the model emitted
        /// malformed JSON.
        args: serde_json::Value,
        /// Wall-clock timestamp.
        at: DateTime<Utc>,
    },
    /// Tool result returned to the model.
    ToolResult {
        /// Tool call id from the provider.
        tool_call_id: String,
        /// Tool name.
        name: String,
        /// Raw tool response text.
        output: String,
        /// Wall-clock timestamp.
        at: DateTime<Utc>,
    },
    /// Execution reached a terminal status.
    ExecutionFinished {
        /// Terminal status.
        status: ExecStatus,
        /// Error text for failed executions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Wall-clock timestamp.
        at: DateTime<Utc>,
    },
}

/// Compact listing entry for one execution JSONL stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecSummary {
    /// Execution id.
    pub exec_id: String,
    /// Project-bucket-relative JSONL path.
    pub path: String,
    /// Task being executed, when the log has a start event.
    pub task_id: Option<String>,
    /// Human-readable task subject, when known.
    pub task_subject: Option<String>,
    /// Terminal status, when the log has finished.
    pub status: Option<ExecStatus>,
    /// Start timestamp, when known.
    pub started_at: Option<DateTime<Utc>>,
    /// Finish timestamp, when known.
    pub finished_at: Option<DateTime<Utc>>,
}

impl Manager {
    /// Construct a manager rooted at the given project bucket.
    #[must_use]
    pub fn new(project_bucket: &Path) -> Self {
        Self {
            dir: project_bucket.join(EXEC_SUBDIR),
        }
    }

    /// Path to the execution directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Start a new execution log and append `execution_started`.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn start(&self, start: ExecStart) -> Result<ExecId> {
        let id = ExecId(Uuid::now_v7());
        self.append(
            &id,
            &ExecEvent::ExecutionStarted {
                exec_id: id.to_string(),
                task_id: start.task_id,
                task_subject: start.task_subject,
                timer_id: start.timer_id,
                timer_title: start.timer_title,
                session_id: start.session.0.to_string(),
                channel_id: start.channel.0,
                at: Utc::now(),
            },
        )
        .await?;
        Ok(id)
    }

    /// Append the execution's final output.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn final_output(&self, id: &ExecId, content: String) -> Result<()> {
        self.append(
            id,
            &ExecEvent::FinalOutput {
                content,
                at: Utc::now(),
            },
        )
        .await
    }

    /// Append a tool call event.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn tool_call(
        &self,
        id: &ExecId,
        tool_call_id: String,
        name: String,
        args: serde_json::Value,
    ) -> Result<()> {
        self.append(
            id,
            &ExecEvent::ToolCall {
                tool_call_id,
                name,
                args,
                at: Utc::now(),
            },
        )
        .await
    }

    /// Append a tool result event.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn tool_result(
        &self,
        id: &ExecId,
        tool_call_id: String,
        name: String,
        output: String,
    ) -> Result<()> {
        self.append(
            id,
            &ExecEvent::ToolResult {
                tool_call_id,
                name,
                output,
                at: Utc::now(),
            },
        )
        .await
    }

    /// Append a terminal execution status.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn finish(
        &self,
        id: &ExecId,
        status: ExecStatus,
        error: Option<String>,
    ) -> Result<()> {
        self.append(
            id,
            &ExecEvent::ExecutionFinished {
                status,
                error,
                at: Utc::now(),
            },
        )
        .await
    }

    /// Load one execution log.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON deserialization errors.
    pub async fn load(&self, id: &ExecId) -> Result<Vec<ExecEvent>> {
        let content = tokio::fs::read_to_string(self.path_for(id)).await?;
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(Error::from))
            .collect()
    }

    /// Load one execution log by UUID string.
    ///
    /// # Errors
    ///
    /// Returns invalid-id, I/O, or JSON deserialization errors.
    pub async fn load_str(&self, raw_id: &str) -> Result<Vec<ExecEvent>> {
        self.load(&ExecId::parse(raw_id)?).await
    }

    /// List execution logs as compact summaries.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON deserialization errors.
    pub async fn list(&self) -> Result<Vec<ExecSummary>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::Io(err)),
        };
        let mut summaries = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let id = ExecId::parse(stem)?;
            let events = self.load(&id).await?;
            summaries.push(summary_from_events(&id, &events));
        }
        summaries.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.exec_id.cmp(&right.exec_id))
        });
        Ok(summaries)
    }

    async fn append(&self, id: &ExecId, event: &ExecEvent) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_for(id))
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Absolute path to an execution JSONL log.
    #[must_use]
    pub fn path_for(&self, id: &ExecId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", id.0))
    }

    /// Project-bucket-relative path to an execution JSONL log.
    #[must_use]
    pub fn relative_path_for(&self, id: &ExecId) -> String {
        format!("{EXEC_SUBDIR}/{}.jsonl", id.0)
    }
}

fn summary_from_events(id: &ExecId, events: &[ExecEvent]) -> ExecSummary {
    let mut summary = ExecSummary {
        exec_id: id.to_string(),
        path: format!("{EXEC_SUBDIR}/{id}.jsonl"),
        task_id: None,
        task_subject: None,
        status: None,
        started_at: None,
        finished_at: None,
    };
    for event in events {
        match event {
            ExecEvent::ExecutionStarted {
                task_id,
                task_subject,
                at,
                ..
            } => {
                summary.task_id = Some(task_id.clone());
                summary.task_subject = Some(task_subject.clone());
                summary.started_at = Some(*at);
            }
            ExecEvent::ExecutionFinished { status, at, .. } => {
                summary.status = Some(*status);
                summary.finished_at = Some(*at);
            }
            ExecEvent::FinalOutput { .. }
            | ExecEvent::ToolCall { .. }
            | ExecEvent::ToolResult { .. } => {}
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-exec-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test]
    async fn exec_history_appends_jsonl_events() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        let exec_id = manager
            .start(ExecStart {
                task_id: "task-1".to_string(),
                task_subject: "Check build".to_string(),
                timer_id: Some("timer-1".to_string()),
                timer_title: Some("Build timer".to_string()),
                session: SessionID(uuid::Uuid::now_v7()),
                channel: ChannelID::new("tui"),
            })
            .await
            .unwrap();
        manager
            .final_output(&exec_id, "Build is green".to_string())
            .await
            .unwrap();
        manager
            .finish(&exec_id, ExecStatus::Succeeded, None)
            .await
            .unwrap();

        assert!(manager.path().ends_with(EXEC_SUBDIR));
        let raw = tokio::fs::read_to_string(manager.path().join(format!("{exec_id}.jsonl")))
            .await
            .unwrap();
        assert!(raw.contains(r#""type":"execution_started""#));
        assert!(raw.contains(r#""type":"execution_finished""#));

        let events = manager.load(&exec_id).await.unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], ExecEvent::ExecutionStarted { .. }));
        assert!(matches!(events[1], ExecEvent::FinalOutput { .. }));
        assert!(matches!(
            events[2],
            ExecEvent::ExecutionFinished {
                status: ExecStatus::Succeeded,
                ..
            }
        ));
        let loaded_by_str = manager.load_str(&exec_id.to_string()).await.unwrap();
        assert_eq!(loaded_by_str.len(), events.len());
        let summaries = manager.list().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].exec_id, exec_id.to_string());
        assert_eq!(summaries[0].task_id.as_deref(), Some("task-1"));
        assert_eq!(summaries[0].status, Some(ExecStatus::Succeeded));
        assert_eq!(summaries[0].path, manager.relative_path_for(&exec_id));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn exec_history_records_tool_events() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        let exec_id = manager
            .start(ExecStart {
                task_id: "task-1".to_string(),
                task_subject: "Check build".to_string(),
                timer_id: None,
                timer_title: None,
                session: SessionID(uuid::Uuid::now_v7()),
                channel: ChannelID::new("tui"),
            })
            .await
            .unwrap();
        manager
            .tool_call(
                &exec_id,
                "call-1".to_string(),
                "file_read".to_string(),
                serde_json::json!({ "path": "README.md" }),
            )
            .await
            .unwrap();
        manager
            .tool_result(
                &exec_id,
                "call-1".to_string(),
                "file_read".to_string(),
                "content".to_string(),
            )
            .await
            .unwrap();

        let events = manager.load(&exec_id).await.unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[1], ExecEvent::ToolCall { .. }));
        assert!(matches!(events[2], ExecEvent::ToolResult { .. }));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
