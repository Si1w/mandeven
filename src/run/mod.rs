//! Machine-readable run history.
//!
//! Runs are execution streams, so their canonical store is JSONL under
//! `<project_bucket>/runs/<run_id>.jsonl`. This module only records the
//! durable event stream; user-visible deliverables should remain
//! Markdown files elsewhere.

pub mod error;

pub use error::{Error, Result};

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::bus::{ChannelID, SessionID};

/// Subdirectory under a project bucket holding run JSONL logs.
pub const RUN_SUBDIR: &str = "runs";

/// Run history manager.
#[derive(Debug, Clone)]
pub struct Manager {
    dir: PathBuf,
}

/// Stable id for one run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunId(pub Uuid);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Input for starting a run log.
#[derive(Clone, Debug)]
pub struct RunStart {
    /// Task being executed.
    pub task_id: String,
    /// Human-readable task subject.
    pub task_subject: String,
    /// Timer that triggered the run, when scheduled.
    pub timer_id: Option<String>,
    /// Human-readable timer title.
    pub timer_title: Option<String>,
    /// Session receiving the run output.
    pub session: SessionID,
    /// Channel receiving the run output.
    pub channel: ChannelID,
}

/// Terminal run status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Run completed without an agent-loop error.
    Succeeded,
    /// Run failed with an agent-loop error.
    Failed,
    /// Run was skipped before execution.
    Skipped,
}

/// One line in a run JSONL file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// Run was accepted by the runtime.
    RunStarted {
        /// Run id.
        run_id: String,
        /// Task being executed.
        task_id: String,
        /// Human-readable task subject.
        task_subject: String,
        /// Timer that triggered the run, when scheduled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timer_id: Option<String>,
        /// Human-readable timer title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timer_title: Option<String>,
        /// Session receiving the run output.
        session_id: String,
        /// Channel receiving the run output.
        channel_id: String,
        /// Wall-clock timestamp.
        at: DateTime<Utc>,
    },
    /// Final assistant-facing output from the run.
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
    /// Run reached a terminal status.
    RunFinished {
        /// Terminal status.
        status: RunStatus,
        /// Error text for failed runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Wall-clock timestamp.
        at: DateTime<Utc>,
    },
}

impl Manager {
    /// Construct a manager rooted at the given project bucket.
    #[must_use]
    pub fn new(project_bucket: &Path) -> Self {
        Self {
            dir: project_bucket.join(RUN_SUBDIR),
        }
    }

    /// Path to the run directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Start a new run log and append `run_started`.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn start(&self, start: RunStart) -> Result<RunId> {
        let id = RunId(Uuid::now_v7());
        self.append(
            &id,
            &RunEvent::RunStarted {
                run_id: id.to_string(),
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

    /// Append the run's final output.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn final_output(&self, id: &RunId, content: String) -> Result<()> {
        self.append(
            id,
            &RunEvent::FinalOutput {
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
        id: &RunId,
        tool_call_id: String,
        name: String,
        args: serde_json::Value,
    ) -> Result<()> {
        self.append(
            id,
            &RunEvent::ToolCall {
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
        id: &RunId,
        tool_call_id: String,
        name: String,
        output: String,
    ) -> Result<()> {
        self.append(
            id,
            &RunEvent::ToolResult {
                tool_call_id,
                name,
                output,
                at: Utc::now(),
            },
        )
        .await
    }

    /// Append a terminal run status.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON serialization errors.
    pub async fn finish(&self, id: &RunId, status: RunStatus, error: Option<String>) -> Result<()> {
        self.append(
            id,
            &RunEvent::RunFinished {
                status,
                error,
                at: Utc::now(),
            },
        )
        .await
    }

    /// Load one run log.
    ///
    /// # Errors
    ///
    /// Returns I/O or JSON deserialization errors.
    pub async fn load(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        let content = tokio::fs::read_to_string(self.path_for(id)).await?;
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(Error::from))
            .collect()
    }

    async fn append(&self, id: &RunId, event: &RunEvent) -> Result<()> {
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

    fn path_for(&self, id: &RunId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", id.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("mandeven-run-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test]
    async fn run_history_appends_jsonl_events() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        let run_id = manager
            .start(RunStart {
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
            .final_output(&run_id, "Build is green".to_string())
            .await
            .unwrap();
        manager
            .finish(&run_id, RunStatus::Succeeded, None)
            .await
            .unwrap();

        let events = manager.load(&run_id).await.unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], RunEvent::RunStarted { .. }));
        assert!(matches!(events[1], RunEvent::FinalOutput { .. }));
        assert!(matches!(
            events[2],
            RunEvent::RunFinished {
                status: RunStatus::Succeeded,
                ..
            }
        ));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn run_history_records_tool_events() {
        let dir = tempdir();
        let manager = Manager::new(&dir);
        let run_id = manager
            .start(RunStart {
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
                &run_id,
                "call-1".to_string(),
                "file_read".to_string(),
                serde_json::json!({ "path": "README.md" }),
            )
            .await
            .unwrap();
        manager
            .tool_result(
                &run_id,
                "call-1".to_string(),
                "file_read".to_string(),
                "content".to_string(),
            )
            .await
            .unwrap();

        let events = manager.load(&run_id).await.unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[1], RunEvent::ToolCall { .. }));
        assert!(matches!(events[2], RunEvent::ToolResult { .. }));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
