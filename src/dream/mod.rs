//! Dream — quiet background review that distills session evidence into memory.
//!
//! Dream is not a normal cron prompt and does not enter the user-visible
//! transcript. Cron-style scheduling owns time; this module owns the semantic
//! review of append-only session events and the idempotent memory upserts.

pub mod engine;
pub mod error;
pub mod store;

pub use engine::DreamEngine;
pub use error::{Error, Result};
pub use store::{SessionCursor, StateFile, Store};

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::llm::{BaseLLMClient, Message, Request, Thinking, Tool};
use crate::memory::{
    self, MemoryDraft, MemoryKind, MemoryQuery, MemoryScope, MemorySource, MemorySourceKind,
};
use crate::session::{self, EventRecord, SessionEvent};

const DEFAULT_SCHEDULE: &str = "0 3 * * *";
const DEFAULT_MIN_INTERVAL_SECS: u64 = 20 * 60 * 60;
const DEFAULT_MAX_EVENTS_PER_RUN: usize = 80;
const DEFAULT_MAX_PROMPT_CHARS: usize = 24_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 2_048;
const MAX_EVENT_CHARS: usize = 2_000;
const MAX_EXISTING_MEMORIES: usize = 24;
const MAX_CANDIDATES: usize = 8;

const DREAM_EXTRACT_TOOL_NAME: &str = "dream_extract";

/// User-tunable Dream knobs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DreamConfig {
    /// Enable the background Dream scheduler.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Cron expression for scheduled Dream review. Evaluated in UTC, matching
    /// the rest of mandeven's cron machinery.
    #[serde(default = "default_schedule")]
    pub schedule: String,
    /// Emit one startup tick so missed nightly reviews can catch up after the
    /// daemon is launched, subject to the cheap gates.
    #[serde(default = "default_run_on_startup")]
    pub run_on_startup: bool,
    /// Minimum seconds between successful review commits.
    #[serde(default = "default_min_interval_secs")]
    pub min_interval_secs: u64,
    /// Maximum append-only session events reviewed in one run.
    #[serde(default = "default_max_events_per_run")]
    pub max_events_per_run: usize,
    /// Approximate character budget for the review prompt's evidence block.
    #[serde(default = "default_max_prompt_chars")]
    pub max_prompt_chars: usize,
    /// Completion token cap for the structured extraction call.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            schedule: default_schedule(),
            run_on_startup: default_run_on_startup(),
            min_interval_secs: default_min_interval_secs(),
            max_events_per_run: default_max_events_per_run(),
            max_prompt_chars: default_max_prompt_chars(),
            max_output_tokens: default_max_output_tokens(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_schedule() -> String {
    DEFAULT_SCHEDULE.to_string()
}

fn default_run_on_startup() -> bool {
    true
}

fn default_min_interval_secs() -> u64 {
    DEFAULT_MIN_INTERVAL_SECS
}

fn default_max_events_per_run() -> usize {
    DEFAULT_MAX_EVENTS_PER_RUN
}

fn default_max_prompt_chars() -> usize {
    DEFAULT_MAX_PROMPT_CHARS
}

fn default_max_output_tokens() -> u32 {
    DEFAULT_MAX_OUTPUT_TOKENS
}

/// One scheduled Dream tick.
#[derive(Clone, Debug)]
pub struct DreamTick {
    /// Tick timestamp.
    pub at: DateTime<Utc>,
    /// Why the tick fired.
    pub reason: DreamTickReason,
}

/// Tick source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DreamTickReason {
    /// Startup catch-up tick.
    Startup,
    /// Cron-scheduled tick.
    Scheduled,
}

/// Inputs for one Dream review attempt.
pub struct RunInput<'a> {
    /// Dream config.
    pub config: &'a DreamConfig,
    /// Dream cursor store.
    pub store: &'a Store,
    /// Session manager for the current project bucket.
    pub sessions: Arc<session::Manager>,
    /// Memory manager for global + project memory.
    pub memory: Arc<memory::Manager>,
    /// LLM client.
    pub client: Arc<dyn BaseLLMClient>,
    /// Upstream model identifier.
    pub model_name: String,
    /// Shared request timeout.
    pub timeout_secs: Option<u64>,
    /// Tick timestamp.
    pub now: DateTime<Utc>,
}

/// Outcome of one Dream attempt.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    /// Final status.
    pub status: RunStatus,
    /// Number of session files with events reviewed.
    pub reviewed_sessions: usize,
    /// Number of event records included in the extraction prompt.
    pub reviewed_events: usize,
    /// Number of memory records created.
    pub memories_created: usize,
    /// Number of existing memory records updated.
    pub memories_updated: usize,
    /// Short explanation for skipped or partial runs.
    pub message: String,
}

/// Dream attempt status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStatus {
    /// Review committed cursor state.
    Succeeded,
    /// No model call was needed.
    Skipped,
}

impl RunOutcome {
    fn skipped(message: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Skipped,
            reviewed_sessions: 0,
            reviewed_events: 0,
            memories_created: 0,
            memories_updated: 0,
            message: message.into(),
        }
    }
}

/// Run one Dream review pass.
///
/// # Errors
///
/// Returns an error when session review, extraction, memory writes, or Dream
/// state persistence fails.
pub async fn run_once(input: RunInput<'_>) -> Result<RunOutcome> {
    if !input.config.enabled {
        return Ok(RunOutcome::skipped("disabled"));
    }
    let Some(lock) = input.store.try_lock(input.now).await? else {
        return Ok(RunOutcome::skipped("lock held"));
    };

    let result = run_locked(input).await;
    let release = lock.release().await;
    match (result, release) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(err), _) | (Ok(_), Err(err)) => Err(err),
    }
}

async fn run_locked(input: RunInput<'_>) -> Result<RunOutcome> {
    let mut state = input.store.load().await?;
    if too_soon(&state, input.now, input.config.min_interval_secs) {
        return Ok(RunOutcome::skipped("min interval not elapsed"));
    }

    let batch = collect_batch(
        input.sessions.as_ref(),
        &state,
        input.config.max_events_per_run,
        input.config.max_prompt_chars,
    )
    .await?;
    if batch.reviewed_events == 0 {
        return Ok(RunOutcome::skipped("no unreviewed session events"));
    }

    let existing = input
        .memory
        .search(MemoryQuery {
            limit: Some(MAX_EXISTING_MEMORIES),
            ..MemoryQuery::default()
        })
        .await?;
    let prompt = build_review_prompt(&batch, &existing);
    let extracted = extract_candidates(&input, prompt).await?;
    let mut memories_created = 0;
    let mut memories_updated = 0;
    let mut skipped_candidates = Vec::new();

    for candidate in extracted.memories.into_iter().take(MAX_CANDIDATES) {
        match candidate.into_draft() {
            Ok(draft) => match input.memory.remember(draft, None).await {
                Ok(outcome) if outcome.updated_existing => memories_updated += 1,
                Ok(_) => memories_created += 1,
                Err(err) => skipped_candidates.push(err.to_string()),
            },
            Err(reason) => skipped_candidates.push(reason),
        }
    }
    let _ = input.memory.profile().await;

    state.last_success_at = Some(input.now);
    for range in &batch.ranges {
        state.sessions.insert(
            range.session_id.clone(),
            SessionCursor {
                reviewed_until_seq: range.processed_until_seq,
                last_seen_seq: range.last_seen_seq,
                updated_at: range.updated_at,
            },
        );
    }
    input.store.save(&state).await?;

    let log = run_log(
        input.now,
        &batch,
        memories_created,
        memories_updated,
        &skipped_candidates,
    );
    let _ = input.store.write_run_log(input.now, &log).await?;

    Ok(RunOutcome {
        status: RunStatus::Succeeded,
        reviewed_sessions: batch.ranges.len(),
        reviewed_events: batch.reviewed_events,
        memories_created,
        memories_updated,
        message: if skipped_candidates.is_empty() {
            "review committed".to_string()
        } else {
            format!(
                "review committed with {} skipped candidate(s)",
                skipped_candidates.len()
            )
        },
    })
}

fn too_soon(state: &StateFile, now: DateTime<Utc>, min_interval_secs: u64) -> bool {
    let Some(last) = state.last_success_at else {
        return false;
    };
    let Ok(elapsed) = (now - last).to_std() else {
        return false;
    };
    elapsed.as_secs() < min_interval_secs
}

#[derive(Debug)]
struct ReviewBatch {
    ranges: Vec<SessionRange>,
    evidence: String,
    reviewed_events: usize,
}

#[derive(Debug)]
struct SessionRange {
    session_id: String,
    title: String,
    from_seq: u64,
    processed_until_seq: u64,
    last_seen_seq: u64,
    updated_at: Option<DateTime<Utc>>,
}

async fn collect_batch(
    sessions: &session::Manager,
    state: &StateFile,
    max_events: usize,
    max_chars: usize,
) -> Result<ReviewBatch> {
    let mut ids = sessions.list().await?;
    ids.sort_by_key(|id| id.0);

    let mut ranges = Vec::new();
    let mut evidence = String::new();
    let mut reviewed_events = 0;

    for id in ids {
        if reviewed_events >= max_events || evidence.len() >= max_chars {
            break;
        }
        let session_key = id.0.to_string();
        let reviewed_until = state
            .sessions
            .get(&session_key)
            .map_or(0, |cursor| cursor.reviewed_until_seq);
        let metadata = sessions.metadata(&id).await?;
        let Some(metadata) = metadata else {
            continue;
        };
        let events = sessions.load_events(&id).await?;
        let last_seen_seq = events.last().map_or(0, |event| event.seq);
        if last_seen_seq <= reviewed_until {
            continue;
        }

        let mut section = String::new();
        let _ = writeln!(
            section,
            "\n## Session {session_key}\nTitle: {}\nUpdated: {}\n",
            metadata.title, metadata.updated_at
        );
        let from_seq = reviewed_until.saturating_add(1);
        let mut processed_until_seq = reviewed_until;
        let mut included = 0;

        for event in events.iter().filter(|event| event.seq > reviewed_until) {
            if reviewed_events >= max_events {
                break;
            }
            let formatted = format_event(event);
            if !section.is_empty() && evidence.len() + section.len() + formatted.len() > max_chars {
                break;
            }
            section.push_str(&formatted);
            processed_until_seq = event.seq;
            reviewed_events += 1;
            included += 1;
        }

        if included == 0 {
            break;
        }
        evidence.push_str(&section);
        ranges.push(SessionRange {
            session_id: session_key,
            title: metadata.title,
            from_seq,
            processed_until_seq,
            last_seen_seq,
            updated_at: Some(metadata.updated_at),
        });
    }

    Ok(ReviewBatch {
        ranges,
        evidence,
        reviewed_events,
    })
}

fn format_event(event: &EventRecord) -> String {
    match &event.event {
        SessionEvent::Message { message } => {
            format!(
                "[seq {} {} {}]\n{}\n\n",
                event.seq,
                event.timestamp,
                role_label(message),
                truncate_chars(&message_text(message), MAX_EVENT_CHARS)
            )
        }
        SessionEvent::Compact {
            summary,
            trigger,
            pre_tokens,
            messages_summarized,
            ..
        } => format!(
            "[seq {} {} compact trigger={trigger:?} pre_tokens={pre_tokens} messages_summarized={messages_summarized}]\n{}\n\n",
            event.seq,
            event.timestamp,
            truncate_chars(summary, MAX_EVENT_CHARS)
        ),
    }
}

fn role_label(message: &Message) -> &'static str {
    match message {
        Message::System { .. } | Message::Compact(_) => "system",
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
        Message::Tool { .. } => "tool",
    }
}

fn message_text(message: &Message) -> String {
    match message {
        Message::System { content } | Message::User { content } | Message::Tool { content, .. } => {
            content.clone()
        }
        Message::Assistant {
            content,
            tool_calls,
            reasoning: _,
        } => {
            let mut out = content.clone().unwrap_or_default();
            if let Some(calls) = tool_calls
                && !calls.is_empty()
            {
                let _ = write!(out, "\nTool calls:");
                for call in calls {
                    let _ = write!(out, "\n- {} {}", call.name, call.arguments);
                }
            }
            out
        }
        Message::Compact(boundary) => boundary.summary.clone(),
    }
}

fn build_review_prompt(batch: &ReviewBatch, existing: &[memory::MemoryMatch]) -> String {
    let mut out = String::from(
        "# Dream Review Input\n\
         Review the append-only session evidence below and extract only durable memories.\n\
         Ignore one-off task progress, raw logs, obvious facts derivable from the repository, \
         and reusable procedures that should become skills later.\n\n",
    );

    if !existing.is_empty() {
        out.push_str("## Existing Active Memories\n");
        for item in existing {
            let memory = &item.memory;
            let _ = writeln!(
                out,
                "- {}/{}: {} — {}",
                memory::scope_name(memory.scope),
                memory::kind_name(memory.kind),
                memory.title,
                memory.summary
            );
        }
        out.push('\n');
    }

    out.push_str("## Session Evidence\n");
    out.push_str(&batch.evidence);
    out
}

async fn extract_candidates(input: &RunInput<'_>, prompt: String) -> Result<DreamExtractArgs> {
    let request = Request {
        messages: vec![
            Message::System {
                content: dream_system_prompt(),
            },
            Message::User { content: prompt },
        ],
        tools: vec![dream_extract_tool()],
        model_name: input.model_name.clone(),
        max_tokens: Some(input.config.max_output_tokens),
        temperature: None,
        timeout_secs: input.timeout_secs,
        thinking: Some(Thinking {
            enabled: false,
            reasoning_effort: None,
        }),
    };
    let response = input.client.complete(request).await?;
    let Some(call) = response
        .tool_calls
        .into_iter()
        .flatten()
        .find(|call| call.name == DREAM_EXTRACT_TOOL_NAME)
    else {
        return Err(Error::InvalidExtraction(
            "missing dream_extract tool call".to_string(),
        ));
    };
    serde_json::from_str(&call.arguments).map_err(Error::from)
}

fn dream_system_prompt() -> String {
    "You are Mandeven Dream, a quiet background memory reviewer. \
     You review session evidence and call dream_extract exactly once. \
     Extract at most 8 durable memories that will reduce future user steering. \
     Use global/user for user facts or preferences. Use global/feedback for \
     corrections about how the assistant should communicate or work. Use \
     project/project for project-local decisions and constraints. Use \
     project/reference only for durable external pointers. Do not save secrets, \
     transient task progress, completed-work diaries, raw logs, or procedures \
     better represented as skills. If nothing is worth keeping, call \
     dream_extract with an empty memories array."
        .to_string()
}

fn dream_extract_tool() -> Tool {
    Tool {
        name: DREAM_EXTRACT_TOOL_NAME.to_string(),
        description: "Return durable memory candidates extracted from session evidence."
            .to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "memories": {
                    "type": "array",
                    "maxItems": MAX_CANDIDATES,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "scope": {
                                "type": "string",
                                "enum": ["global", "project"]
                            },
                            "kind": {
                                "type": "string",
                                "enum": ["user", "feedback", "project", "reference"]
                            },
                            "title": {
                                "type": "string",
                                "description": "Stable short title for idempotent upsert."
                            },
                            "summary": {
                                "type": "string",
                                "description": "One concise sentence suitable for prompt snapshots."
                            },
                            "body": {
                                "type": "string",
                                "description": "Details and caveats. Keep concise."
                            },
                            "tags": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "session_id": {
                                "type": "string",
                                "description": "Supporting session UUID when known."
                            },
                            "seq": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Supporting event sequence when known."
                            },
                            "quote": {
                                "type": "string",
                                "description": "Short quote or paraphrase supporting this memory."
                            }
                        },
                        "required": ["scope", "kind", "title", "summary", "body"]
                    }
                }
            },
            "required": ["memories"]
        }),
    }
}

#[derive(Debug, Default, Deserialize)]
struct DreamExtractArgs {
    #[serde(default)]
    memories: Vec<DreamMemoryCandidate>,
}

#[derive(Debug, Deserialize)]
struct DreamMemoryCandidate {
    scope: String,
    kind: String,
    title: String,
    summary: String,
    body: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    quote: Option<String>,
}

impl DreamMemoryCandidate {
    fn into_draft(self) -> std::result::Result<MemoryDraft, String> {
        let kind = parse_kind(&self.kind)?;
        let mut scope = parse_scope(&self.scope)?;
        if kind == MemoryKind::User {
            scope = MemoryScope::Global;
        }
        let mut tags = self.tags;
        tags.push("dream".to_string());
        let quote = self
            .quote
            .or_else(|| self.seq.map(|seq| format!("session event seq {seq}")));
        Ok(MemoryDraft {
            scope,
            kind,
            title: truncate_chars(self.title.trim(), 120),
            summary: truncate_chars(self.summary.trim(), 320),
            body: truncate_chars(self.body.trim(), 2_000),
            tags: tags
                .into_iter()
                .map(|tag| truncate_chars(tag.trim(), 40))
                .filter(|tag| !tag.is_empty())
                .take(12)
                .collect(),
            source: MemorySource {
                kind: MemorySourceKind::AssistantObserved,
                session_id: self.session_id,
                quote: quote.map(|q| truncate_chars(q.trim(), 320)),
            },
            review_after: None,
        })
    }
}

fn parse_scope(raw: &str) -> std::result::Result<MemoryScope, String> {
    match raw {
        "global" => Ok(MemoryScope::Global),
        "project" => Ok(MemoryScope::Project),
        other => Err(format!("invalid memory scope {other:?}")),
    }
}

fn parse_kind(raw: &str) -> std::result::Result<MemoryKind, String> {
    match raw {
        "user" => Ok(MemoryKind::User),
        "feedback" => Ok(MemoryKind::Feedback),
        "project" => Ok(MemoryKind::Project),
        "reference" => Ok(MemoryKind::Reference),
        other => Err(format!("invalid memory kind {other:?}")),
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return value.chars().take(max_chars).collect();
    }
    let mut out: String = value.chars().take(max_chars - 3).collect();
    out.push_str("...");
    out
}

fn run_log(
    at: DateTime<Utc>,
    batch: &ReviewBatch,
    memories_created: usize,
    memories_updated: usize,
    skipped_candidates: &[String],
) -> String {
    let mut out = format!(
        "# Dream Run\n\n- Time: {at}\n- Reviewed sessions: {}\n- Reviewed events: {}\n- Memories created: {memories_created}\n- Memories updated: {memories_updated}\n",
        batch.ranges.len(),
        batch.reviewed_events
    );
    if !batch.ranges.is_empty() {
        out.push_str("\n## Session Ranges\n");
        for range in &batch.ranges {
            let _ = writeln!(
                out,
                "- {} `{}`: seq {}..{} of {}",
                range.title,
                range.session_id,
                range.from_seq,
                range.processed_until_seq,
                range.last_seen_seq
            );
        }
    }
    if !skipped_candidates.is_empty() {
        out.push_str("\n## Skipped Candidates\n");
        for reason in skipped_candidates {
            let _ = writeln!(out, "- {reason}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::bus::{ChannelID, SessionID};
    use crate::llm::{FinishReason, ToolCall, Usage};

    struct MockClient {
        args: String,
    }

    #[async_trait]
    impl BaseLLMClient for MockClient {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn api_key_env(&self) -> &'static str {
            "MOCK_API_KEY"
        }

        async fn complete(&self, _req: Request) -> crate::llm::Result<crate::llm::Response> {
            Ok(crate::llm::Response {
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call-1".to_string(),
                    name: DREAM_EXTRACT_TOOL_NAME.to_string(),
                    arguments: self.args.clone(),
                }]),
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cache_hit_tokens: None,
                    cache_miss_tokens: None,
                },
                finish_reason: FinishReason::Stop,
                thinking: None,
            })
        }

        async fn stream(&self, _req: Request) -> crate::llm::Result<crate::llm::ResponseStream> {
            unreachable!("dream uses complete")
        }
    }

    fn tempdir(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mandeven-dream-{label}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn run_once_extracts_memory_and_advances_cursor() {
        let data_dir = tempdir("data");
        let project_dir = tempdir("project");
        let sessions = Arc::new(session::Manager::new(project_dir.clone()).await.unwrap());
        let memory = Arc::new(memory::Manager::new(&data_dir, &project_dir));
        let session_id = SessionID::new();
        sessions
            .create(&session_id, "dream test".to_string(), ChannelID::new("tui"))
            .await
            .unwrap();
        sessions
            .append(
                &session_id,
                Message::User {
                    content: "以后这类设计 review 用中文，直接说 tradeoff。".to_string(),
                },
            )
            .await
            .unwrap();

        let args = serde_json::to_string(&json!({
            "memories": [{
                "scope": "global",
                "kind": "feedback",
                "title": "Chinese design reviews",
                "summary": "The user prefers Chinese design reviews with concrete tradeoffs.",
                "body": "When reviewing design work, answer in Chinese and focus on concrete tradeoffs.",
                "tags": ["style"],
                "session_id": session_id.0.to_string(),
                "seq": 1,
                "quote": "用中文，直接说 tradeoff"
            }]
        }))
        .unwrap();

        let cfg = DreamConfig {
            min_interval_secs: 1,
            ..DreamConfig::default()
        };
        let store = Store::new(&project_dir);
        let now = Utc::now();
        let outcome = run_once(RunInput {
            config: &cfg,
            store: &store,
            sessions: sessions.clone(),
            memory: memory.clone(),
            client: Arc::new(MockClient { args }),
            model_name: "mock-model".to_string(),
            timeout_secs: None,
            now,
        })
        .await
        .unwrap();

        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.reviewed_sessions, 1);
        assert_eq!(outcome.reviewed_events, 1);
        assert_eq!(outcome.memories_created, 1);

        let state = store.load().await.unwrap();
        let cursor = state.sessions.get(&session_id.0.to_string()).unwrap();
        assert_eq!(cursor.reviewed_until_seq, 1);

        let memories = memory
            .search(MemoryQuery {
                query: Some("Chinese design".to_string()),
                ..MemoryQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].memory.kind, MemoryKind::Feedback);

        let _ = tokio::fs::remove_dir_all(data_dir).await;
        let _ = tokio::fs::remove_dir_all(project_dir).await;
    }
}
