//! Session persistence — one JSONL file per session.
//!
//! Sessions are scoped per launch directory, so the canonical location of
//! `<base_dir>` is the project bucket `~/.mandeven/projects/<sanitized-cwd>/`
//! (see [`crate::config::project_bucket`]).
//!
//! File layout inside the bucket:
//!
//! ```text
//! <base_dir>/<uuid>.jsonl:
//!   {"_type":"metadata","title":"...","created_at":"...","updated_at":"...","memory_snapshot":"..."}
//!   {"seq":1,"timestamp":"...","_type":"message","role":"user","content":"hi"}
//!   {"seq":2,"timestamp":"...","_type":"message","role":"assistant","content":"hello"}
//!   {"seq":3,"timestamp":"...","_type":"compact","summary":"...","messages":[...]}
//! ```
//!
//! The first line is a [`Metadata`] block tagged `_type:"metadata"`;
//! subsequent lines are append-only [`EventRecord`]s. [`SessionEvent::Message`]
//! stores one chronological [`Message`]; [`SessionEvent::Compact`] stores the
//! compact summary metadata plus replay state produced by compaction without
//! deleting earlier evidence.
//!
//! Appends are written as one JSONL record at the end of the file.
//! Compaction appends a compact event rather than rewriting older records. Full
//! rewrites are reserved for metadata-only updates such as backfilling a frozen
//! memory snapshot; they must preserve the event stream verbatim.
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
use crate::llm::{CompactBoundary, CompactTrigger, Message};

/// Marker emitted on the metadata line's `_type` field.
const METADATA_MARKER: &str = "metadata";

/// Session-level metadata stored as the first line of the file.
///
/// Schema note: the `channel` field is non-optional — every session
/// must record which channel produced it, so `/list`-style commands
/// can filter accurately once multiple channels coexist. Session
/// files written before this field was introduced will fail to
/// parse; callers are expected to delete them (the project bucket is
/// gitignored and holds only local state).
//
// TODO(multi-peer):   peer_id:  Option<String>
// TODO(multi-agent):  agent_id: Option<String>
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
    /// When the session was last modified. Appends do not rewrite the
    /// metadata line; readers derive a fresher value from the last
    /// record timestamp when needed.
    pub updated_at: DateTime<Utc>,
    /// Frozen memory/profile snapshot captured when the session starts.
    ///
    /// `None` means this session predates the field or capture failed and
    /// should be retried. `Some("")` means capture succeeded but there was no
    /// memory to surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_snapshot: Option<String>,
}

/// One replayable message entry returned by [`Manager::load`].
///
/// This is a projection of the append-only event stream: after a compact event,
/// replay starts from the compacted message list stored in that event and then
/// includes later message events.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Record {
    /// Event sequence that made this message visible in the replay stream.
    pub seq: u64,
    /// Conversation message.
    pub message: Message,
    /// When this message was appended.
    pub timestamp: DateTime<Utc>,
}

/// One append-only event line in the session file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventRecord {
    /// Monotonic per-session sequence number. Starts at 1.
    pub seq: u64,
    /// When this event was appended.
    pub timestamp: DateTime<Utc>,
    /// Event payload.
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// Append-only session events.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "_type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A normal conversation message appended by the agent loop.
    Message {
        /// Conversation message.
        #[serde(flatten)]
        message: Message,
    },
    /// A compaction boundary. Earlier events remain on disk; replay state is
    /// reset to this compacted message list. Summary fields are duplicated at
    /// the top level so dream/review agents can grep compact output without
    /// parsing nested replay messages.
    Compact {
        /// Summary text generated by the compact LLM call.
        summary: String,
        /// Whether the compaction was kicked off manually or automatically.
        trigger: CompactTrigger,
        /// Token count estimated for the pre-compact region.
        pre_tokens: u32,
        /// Number of messages summarized away.
        messages_summarized: usize,
        /// Messages the next LLM request should replay after compaction.
        messages: Vec<Message>,
    },
}

/// Manages session persistence across a daemon lifetime.
///
/// One JSONL file per session. A per-session async mutex protects
/// against concurrent writers.
pub struct Manager {
    base_dir: PathBuf,
    locks: Mutex<HashMap<SessionID, Arc<Mutex<()>>>>,
    next_seq: Mutex<HashMap<SessionID, u64>>,
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
    events: Vec<EventRecord>,
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
            next_seq: Mutex::new(HashMap::new()),
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
        self.create_with_memory_snapshot(id, title, channel, None)
            .await
    }

    /// Write a fresh session file with a frozen memory snapshot.
    ///
    /// See [`Self::create`]; the snapshot is metadata rather than a message so
    /// it can be injected into the transient system prompt without becoming
    /// part of the user-visible transcript.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] or [`Error::Json`] on filesystem or
    /// serialization failure.
    pub async fn create_with_memory_snapshot(
        &self,
        id: &SessionID,
        title: String,
        channel: ChannelID,
        memory_snapshot: Option<String>,
    ) -> Result<()> {
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;

        let now = Utc::now();
        let path = self.session_path(id);
        let state = State {
            metadata: Metadata {
                title,
                channel,
                created_at: now,
                updated_at: now,
                memory_snapshot,
            },
            events: Vec::new(),
        };
        self.write_state(&path, &state).await?;
        self.set_cached_next_seq(id, 1).await;
        Ok(())
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

    /// Replace the session's frozen memory snapshot in metadata.
    ///
    /// Used to lazily backfill sessions created before this field existed, or
    /// to retry capture after a transient memory-store error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the session file does not exist, and
    /// [`Error::Io`], [`Error::Json`], or [`Error::InvalidFormat`] on
    /// filesystem or parsing failure.
    pub async fn set_memory_snapshot(
        &self,
        id: &SessionID,
        memory_snapshot: Option<String>,
    ) -> Result<()> {
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;

        let path = self.session_path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Err(Error::NotFound(id.clone()));
        }

        let mut state = self.read_state(&path).await?;
        state.metadata.memory_snapshot = memory_snapshot;
        self.write_state(&path, &state).await
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
        let seq = self.allocate_seq(id, &path).await?;
        let record = Record {
            seq,
            message: msg,
            timestamp: now,
        };
        let event = EventRecord {
            seq: record.seq,
            timestamp: record.timestamp,
            event: SessionEvent::Message {
                message: record.message,
            },
        };
        self.append_event_line(&path, &event).await
    }

    /// Append a compaction event that resets replay state without deleting
    /// earlier records from the session file.
    ///
    /// Used by [`crate::agent::compact`]: a successful compaction produces a
    /// new `Vec<Message>` (summary boundary + preserve region) that becomes the
    /// current replay state. The raw pre-compact messages remain available in
    /// earlier append-only events for later dream/review passes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when the session file doesn't
    /// exist (call [`Self::create`] first), and [`Error::Io`],
    /// [`Error::Json`], or [`Error::InvalidFormat`] on filesystem or
    /// parsing failure.
    pub async fn append_compaction(&self, id: &SessionID, messages: Vec<Message>) -> Result<()> {
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;

        let path = self.session_path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Err(Error::NotFound(id.clone()));
        }

        let now = Utc::now();
        let seq = self.allocate_seq(id, &path).await?;
        let compact = compact_boundary_from_messages(&messages)?;
        let event = EventRecord {
            seq,
            timestamp: now,
            event: SessionEvent::Compact {
                summary: compact.summary.clone(),
                trigger: compact.trigger,
                pre_tokens: compact.pre_tokens,
                messages_summarized: compact.messages_summarized,
                messages,
            },
        };
        self.append_event_line(&path, &event).await
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
        Ok(state.replay_records())
    }

    /// Load the raw append-only event stream for review/dream consumers.
    ///
    /// Unlike [`Self::load`], this does not apply compact events to produce a
    /// replay view. It returns every message and compact event still present on
    /// disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`], [`Error::Json`], or
    /// [`Error::InvalidFormat`] on filesystem or parsing failure.
    pub async fn load_events(&self, id: &SessionID) -> Result<Vec<EventRecord>> {
        let path = self.session_path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(Vec::new());
        }
        let state = self.read_state(&path).await?;
        Ok(state.events)
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

    /// Enumerate session ids whose session file mtime is newer than `since`.
    ///
    /// Passing `None` returns every session file. Files whose stem does not
    /// parse as a UUID are ignored silently.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on filesystem failure.
    pub async fn list_touched_since(&self, since: Option<DateTime<Utc>>) -> Result<Vec<SessionID>> {
        let mut dir = tokio::fs::read_dir(&self.base_dir).await?;
        let mut ids = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(uuid) = Uuid::parse_str(stem) else {
                continue;
            };
            let modified = entry.metadata().await?.modified()?;
            let modified_at = DateTime::<Utc>::from(modified);
            if since.is_none_or(|since| modified_at > since) {
                ids.push(SessionID(uuid));
            }
        }
        ids.sort_by_key(|id| id.0);
        Ok(ids)
    }

    fn session_path(&self, id: &SessionID) -> PathBuf {
        self.base_dir.join(format!("{}.jsonl", id.0))
    }

    fn tmp_path(path: &Path) -> PathBuf {
        path.with_extension("jsonl.tmp")
    }

    async fn lock_for(&self, id: &SessionID) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn set_cached_next_seq(&self, id: &SessionID, next: u64) {
        self.next_seq.lock().await.insert(id.clone(), next);
    }

    async fn allocate_seq(&self, id: &SessionID, path: &Path) -> Result<u64> {
        {
            let mut next_seq = self.next_seq.lock().await;
            if let Some(next) = next_seq.get_mut(id) {
                let seq = *next;
                *next = (*next).saturating_add(1);
                return Ok(seq);
            }
        }

        let state = self.read_state(path).await?;
        let next = state.events.last().map_or(1, |r| r.seq.saturating_add(1));
        let mut next_seq = self.next_seq.lock().await;
        let entry = next_seq.entry(id.clone()).or_insert(next);
        let seq = *entry;
        *entry = (*entry).saturating_add(1);
        Ok(seq)
    }

    async fn append_event_line(&self, path: &Path, event: &EventRecord) -> Result<()> {
        let mut line = serde_json::to_string(event)?;
        line.push('\n');

        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
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

        let events = lines
            .map(|l| serde_json::from_str(l).map_err(Error::from))
            .collect::<Result<Vec<EventRecord>>>()?;
        validate_event_sequence(&events)?;
        if let Some(last) = events.last()
            && last.timestamp > meta_parsed.meta.updated_at
        {
            meta_parsed.meta.updated_at = last.timestamp;
        }

        Ok(State {
            metadata: meta_parsed.meta,
            events,
        })
    }

    async fn write_state(&self, path: &Path, state: &State) -> Result<()> {
        let mut content = String::new();

        let meta_line = MetadataLineWrite {
            marker: METADATA_MARKER,
            meta: &state.metadata,
        };
        content.push_str(&serde_json::to_string(&meta_line)?);
        content.push('\n');

        for event in &state.events {
            content.push_str(&serde_json::to_string(event)?);
            content.push('\n');
        }

        let tmp = Self::tmp_path(path);
        let final_path = path;
        tokio::fs::write(&tmp, content.as_bytes()).await?;
        tokio::fs::rename(&tmp, final_path).await?;
        Ok(())
    }
}

impl State {
    fn replay_records(&self) -> Vec<Record> {
        let mut out = Vec::new();
        for event in &self.events {
            match &event.event {
                SessionEvent::Message { message } => out.push(Record {
                    seq: event.seq,
                    message: message.clone(),
                    timestamp: event.timestamp,
                }),
                SessionEvent::Compact { messages, .. } => {
                    out = messages
                        .iter()
                        .cloned()
                        .map(|message| Record {
                            seq: event.seq,
                            message,
                            timestamp: event.timestamp,
                        })
                        .collect();
                }
            }
        }
        out
    }
}

fn compact_boundary_from_messages(messages: &[Message]) -> Result<&CompactBoundary> {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Compact(boundary) => Some(boundary),
            _ => None,
        })
        .ok_or_else(|| {
            Error::InvalidFormat(
                "compact event requires replay messages to include a compact boundary".into(),
            )
        })
}

fn validate_event_sequence(events: &[EventRecord]) -> Result<()> {
    for (idx, event) in events.iter().enumerate() {
        let expected = idx as u64 + 1;
        if event.seq != expected {
            return Err(Error::InvalidFormat(format!(
                "session event seq {} at position {}, expected {expected}",
                event.seq,
                idx + 1
            )));
        }
    }
    Ok(())
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

        let path = manager.session_path(&id);
        let content = tokio::fs::read_to_string(path).await.unwrap();
        assert_eq!(content.lines().count(), 3);
        assert!(content.contains("\"seq\":1"));
        assert!(content.contains("\"seq\":2"));
        assert!(content.contains("\"_type\":\"message\""));

        let records = manager.load(&id).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].seq, 2);
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

    #[tokio::test]
    async fn new_sessions_are_written_directly_under_project_bucket() {
        let dir = temp_session_dir();
        let manager = Manager::new(dir.clone()).await.unwrap();
        let id = SessionID::new();
        manager
            .create(&id, "test".to_string(), ChannelID::new("tui"))
            .await
            .unwrap();

        let path = manager.session_path(&id);
        assert_eq!(path.parent().unwrap(), dir.as_path());
        assert!(tokio::fs::try_exists(&path).await.unwrap());

        let listed = manager.list().await.unwrap();
        assert_eq!(listed, vec![id]);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn compaction_appends_event_and_replay_uses_latest_compact_state() {
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
                    content: "old".to_string(),
                },
            )
            .await
            .unwrap();
        manager
            .append_compaction(
                &id,
                vec![
                    Message::Compact(crate::llm::CompactBoundary {
                        summary: "old summary".to_string(),
                        trigger: crate::llm::CompactTrigger::Manual,
                        pre_tokens: 10,
                        messages_summarized: 1,
                    }),
                    Message::User {
                        content: "kept".to_string(),
                    },
                ],
            )
            .await
            .unwrap();
        manager
            .append(
                &id,
                Message::Assistant {
                    content: Some("new".to_string()),
                    tool_calls: None,
                    reasoning: None,
                },
            )
            .await
            .unwrap();
        manager
            .append_compaction(
                &id,
                vec![
                    Message::Compact(crate::llm::CompactBoundary {
                        summary: "second summary".to_string(),
                        trigger: crate::llm::CompactTrigger::Auto,
                        pre_tokens: 20,
                        messages_summarized: 3,
                    }),
                    Message::User {
                        content: "latest".to_string(),
                    },
                ],
            )
            .await
            .unwrap();

        let events = manager.load_events(&id).await.unwrap();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0].event, SessionEvent::Message { .. }));
        assert!(matches!(
            &events[1].event,
            SessionEvent::Compact { summary, trigger, messages_summarized, .. }
                if summary == "old summary"
                    && *trigger == crate::llm::CompactTrigger::Manual
                    && *messages_summarized == 1
        ));
        assert_eq!(events[2].seq, 3);
        assert!(matches!(
            &events[3].event,
            SessionEvent::Compact { summary, trigger, messages_summarized, .. }
                if summary == "second summary"
                    && *trigger == crate::llm::CompactTrigger::Auto
                    && *messages_summarized == 3
        ));

        let replay = manager.load(&id).await.unwrap();
        assert_eq!(replay.len(), 2);
        assert!(matches!(replay[0].message, Message::Compact(_)));
        assert!(matches!(
            &replay[1].message,
            Message::User { content } if content == "latest"
        ));

        let path = manager.session_path(&id);
        let content = tokio::fs::read_to_string(path).await.unwrap();
        assert_eq!(content.lines().count(), 5);
        assert!(content.contains("\"_type\":\"compact\""));
        assert!(content.contains("\"summary\":\"old summary\""));
        assert!(content.contains("\"summary\":\"second summary\""));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn memory_snapshot_is_stored_in_metadata() {
        let dir = temp_session_dir();
        let manager = Manager::new(dir.clone()).await.unwrap();
        let id = SessionID::new();
        manager
            .create_with_memory_snapshot(
                &id,
                "test".to_string(),
                ChannelID::new("tui"),
                Some("snapshot".to_string()),
            )
            .await
            .unwrap();

        let metadata = manager.metadata(&id).await.unwrap().unwrap();
        assert_eq!(metadata.memory_snapshot.as_deref(), Some("snapshot"));

        manager
            .set_memory_snapshot(&id, Some("updated".to_string()))
            .await
            .unwrap();
        let metadata = manager.metadata(&id).await.unwrap().unwrap();
        assert_eq!(metadata.memory_snapshot.as_deref(), Some("updated"));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
