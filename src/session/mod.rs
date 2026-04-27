//! Session persistence — one JSONL file per session.
//!
//! Sessions are scoped per launch directory, so the canonical location
//! of `<base_dir>` is the project bucket
//! `~/.mandeven/projects/<sanitized-cwd>/` (see
//! [`crate::config::project_bucket`]). One mandeven install therefore
//! tracks every project's sessions side-by-side without their files
//! ever colliding.
//!
//! File layout inside the bucket:
//!
//! ```text
//! <base_dir>/<uuid>.jsonl:
//!   {"_type":"metadata","title":"...","created_at":"...","updated_at":"..."}
//!   {"role":"user","content":"hi","timestamp":"..."}
//!   {"role":"assistant","content":"hello","timestamp":"..."}
//! ```
//!
//! The first line is a [`Metadata`] block tagged `_type:"metadata"`;
//! subsequent lines are [`Record`]s — a chronological [`Message`] plus
//! its wall-clock timestamp.
//!
//! Appends are written as one JSONL record at the end of the file.
//! Full rewrites are reserved for operations that intentionally
//! replace history, such as compaction; those still write to
//! `<uuid>.jsonl.tmp` and `rename` into place.
//!
//! A per-session async [`tokio::sync::Mutex`] serializes writes
//! (necessary once background tasks such as auto-compaction start
//! modifying sessions concurrently with the agent loop).

pub mod error;

pub use error::{Error, Result};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::bus::{ChannelID, SessionID};
use crate::llm::Message;

/// Marker emitted on the metadata line's `_type` field.
const METADATA_MARKER: &str = "metadata";

/// Session-level metadata stored as the first line of the file.
///
/// Schema note: the `channel` field is non-optional — every session
/// must record which channel produced it, so `/list`-style commands
/// can filter accurately once multiple channels coexist. Session
/// files written before this field was introduced will fail to
/// parse; callers are expected to delete them (the `sessions/`
/// directory is gitignored and holds only local state).
//
// TODO(multi-peer):   peer_id:  Option<String>
// TODO(multi-agent):  agent_id: Option<String>
// TODO(compaction):   summary:  Option<String>, last_consolidated: usize
// TODO(tagging):      tags:     Vec<String>
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Metadata {
    /// Human-readable title, typically generated from the first user
    /// query by the agent.
    pub title: String,
    /// Channel that produced this session. Used by the gateway's
    /// `/list` to scope output to a single channel's sessions.
    pub channel: ChannelID,
    /// When the session was first created.
    pub created_at: DateTime<Utc>,
    /// When the session was last modified (bumped on every append).
    pub updated_at: DateTime<Utc>,
}

/// One message entry in a session file, carrying its wall-clock
/// timestamp alongside the [`Message`] itself.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Record {
    /// Conversation message.
    #[serde(flatten)]
    pub message: Message,
    /// When this message was appended.
    pub timestamp: DateTime<Utc>,
}

/// Manages session persistence across a daemon lifetime.
///
/// One JSONL file per session. A per-session async mutex protects
/// against concurrent writers.
pub struct Manager {
    base_dir: PathBuf,
    locks: Mutex<HashMap<SessionID, Arc<Mutex<()>>>>,
}

/// Serialization wrapper that prepends `_type:"metadata"`.
#[derive(Serialize)]
struct MetadataLineWrite<'a> {
    #[serde(rename = "_type")]
    marker: &'static str,
    #[serde(flatten)]
    meta: &'a Metadata,
}

/// Deserialization wrapper that strips the `_type` field.
#[derive(Deserialize)]
struct MetadataLineRead {
    #[serde(rename = "_type")]
    marker: String,
    #[serde(flatten)]
    meta: Metadata,
}

/// In-memory representation of a session file's full contents.
struct State {
    metadata: Metadata,
    records: Vec<Record>,
}

impl Manager {
    /// Ensure `base_dir` exists and wrap it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory cannot be created.
    pub async fn new(base_dir: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&base_dir).await?;
        Ok(Self {
            base_dir,
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// Write a fresh session file with the given title, originating
    /// channel, and no messages. Overwrites any existing file for
    /// the same id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] or [`Error::Json`] on filesystem or
    /// serialization failure.
    pub async fn create(&self, id: &SessionID, title: String, channel: ChannelID) -> Result<()> {
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;

        let now = Utc::now();
        let state = State {
            metadata: Metadata {
                title,
                channel,
                created_at: now,
                updated_at: now,
            },
            records: Vec::new(),
        };
        self.write_state(id, &state).await
    }

    /// Read a session's metadata.
    ///
    /// Returns `None` when no file exists for `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`], [`Error::Json`], or
    /// [`Error::InvalidFormat`] on filesystem, parsing, or structural
    /// failure.
    pub async fn metadata(&self, id: &SessionID) -> Result<Option<Metadata>> {
        let path = self.session_path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        let state = self.read_state(&path).await?;
        Ok(Some(state.metadata))
    }

    /// Append one message to a session as a single JSONL line.
    ///
    /// The metadata line is not rewritten on append. Readers derive
    /// `updated_at` from the last record timestamp so `/list` still
    /// sorts by real activity without turning every append into a full
    /// file rewrite.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the session file does not
    /// exist (call [`Self::create`] first), and [`Error::Io`],
    /// [`Error::Json`], or [`Error::InvalidFormat`] on filesystem or
    /// parsing failure.
    pub async fn append(&self, id: &SessionID, msg: Message) -> Result<()> {
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;

        let path = self.session_path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Err(Error::NotFound(id.clone()));
        }

        let now = Utc::now();
        let record = Record {
            message: msg,
            timestamp: now,
        };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');

        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Replace the entire message log of a session in one shot.
    ///
    /// Used by [`crate::agent::compact`]: a successful compaction produces a
    /// new `Vec<Message>` (system prompts + summary boundary +
    /// preserve region) that is meant to **overwrite** the JSONL
    /// rather than append to it. All records share a single
    /// timestamp because the compaction is one logical event.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the session file doesn't
    /// exist (call [`Self::create`] first), and [`Error::Io`],
    /// [`Error::Json`], or [`Error::InvalidFormat`] on filesystem or
    /// parsing failure.
    pub async fn replace_messages(&self, id: &SessionID, messages: Vec<Message>) -> Result<()> {
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;

        let path = self.session_path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Err(Error::NotFound(id.clone()));
        }

        let mut state = self.read_state(&path).await?;
        let now = Utc::now();
        state.records = messages
            .into_iter()
            .map(|message| Record {
                message,
                timestamp: now,
            })
            .collect();
        state.metadata.updated_at = now;

        self.write_state(id, &state).await
    }

    /// Load the full chronological history of a session.
    ///
    /// Returns an empty `Vec` when the session file is absent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`], [`Error::Json`], or
    /// [`Error::InvalidFormat`] on filesystem or parsing failure.
    pub async fn load(&self, id: &SessionID) -> Result<Vec<Record>> {
        let path = self.session_path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(Vec::new());
        }
        let state = self.read_state(&path).await?;
        Ok(state.records)
    }

    /// Enumerate session ids currently present in the store.
    ///
    /// Files whose stem does not parse as a UUID are ignored
    /// silently.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on filesystem failure.
    pub async fn list(&self) -> Result<Vec<SessionID>> {
        let mut dir = tokio::fs::read_dir(&self.base_dir).await?;
        let mut ids = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(uuid) = Uuid::parse_str(stem)
            {
                ids.push(SessionID(uuid));
            }
        }
        Ok(ids)
    }

    fn session_path(&self, id: &SessionID) -> PathBuf {
        self.base_dir.join(format!("{}.jsonl", id.0))
    }

    fn tmp_path(&self, id: &SessionID) -> PathBuf {
        self.base_dir.join(format!("{}.jsonl.tmp", id.0))
    }

    async fn lock_for(&self, id: &SessionID) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn read_state(&self, path: &Path) -> Result<State> {
        let content = tokio::fs::read_to_string(path).await?;
        let mut lines = content.lines().filter(|l| !l.is_empty());

        let meta_line = lines
            .next()
            .ok_or_else(|| Error::InvalidFormat("session file is empty".into()))?;
        let mut meta_parsed: MetadataLineRead = serde_json::from_str(meta_line)?;
        if meta_parsed.marker != METADATA_MARKER {
            return Err(Error::InvalidFormat(format!(
                "first line _type is '{}', expected '{METADATA_MARKER}'",
                meta_parsed.marker
            )));
        }

        let records = lines
            .map(|l| serde_json::from_str(l).map_err(Error::from))
            .collect::<Result<Vec<Record>>>()?;
        if let Some(last) = records.last()
            && last.timestamp > meta_parsed.meta.updated_at
        {
            meta_parsed.meta.updated_at = last.timestamp;
        }

        Ok(State {
            metadata: meta_parsed.meta,
            records,
        })
    }

    async fn write_state(&self, id: &SessionID, state: &State) -> Result<()> {
        let mut content = String::new();

        let meta_line = MetadataLineWrite {
            marker: METADATA_MARKER,
            meta: &state.metadata,
        };
        content.push_str(&serde_json::to_string(&meta_line)?);
        content.push('\n');

        for record in &state.records {
            content.push_str(&serde_json::to_string(record)?);
            content.push('\n');
        }

        let tmp = self.tmp_path(id);
        let final_path = self.session_path(id);
        tokio::fs::write(&tmp, content.as_bytes()).await?;
        tokio::fs::rename(&tmp, &final_path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session_dir() -> PathBuf {
        std::env::temp_dir().join(format!("mandeven-session-test-{}", Uuid::now_v7()))
    }

    #[tokio::test]
    async fn append_writes_jsonl_records_in_order() {
        let dir = temp_session_dir();
        let manager = Manager::new(dir.clone()).await.unwrap();
        let id = SessionID::new();
        manager
            .create(&id, "test".to_string(), ChannelID::new("tui"))
            .await
            .unwrap();

        manager
            .append(
                &id,
                Message::User {
                    content: "one".to_string(),
                },
            )
            .await
            .unwrap();
        manager
            .append(
                &id,
                Message::Assistant {
                    content: Some("two".to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
            )
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(manager.session_path(&id))
            .await
            .unwrap();
        assert_eq!(content.lines().count(), 3);

        let records = manager.load(&id).await.unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(
            &records[0].message,
            Message::User { content } if content == "one"
        ));
        assert!(matches!(
            &records[1].message,
            Message::Assistant { content: Some(content), .. } if content == "two"
        ));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn metadata_updated_at_tracks_last_appended_record() {
        let dir = temp_session_dir();
        let manager = Manager::new(dir.clone()).await.unwrap();
        let id = SessionID::new();
        manager
            .create(&id, "test".to_string(), ChannelID::new("tui"))
            .await
            .unwrap();
        manager
            .append(
                &id,
                Message::User {
                    content: "hello".to_string(),
                },
            )
            .await
            .unwrap();

        let records = manager.load(&id).await.unwrap();
        let metadata = manager.metadata(&id).await.unwrap().unwrap();

        assert_eq!(metadata.updated_at, records[0].timestamp);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
