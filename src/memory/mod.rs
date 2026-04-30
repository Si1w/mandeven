//! Durable curated memory and a lightweight derived user profile.
//!
//! Memory is distinct from session history, task state, cron schedules, and
//! `AGENTS.md` instructions:
//!
//! - session history is the transcript;
//! - task state is current work coordination;
//! - cron state is future autonomous triggers;
//! - `AGENTS.md` is user-authored stable instruction;
//! - memory stores compact facts, preferences, feedback, and reference pointers
//!   that remain useful across sessions and are not easily re-derived.

pub mod error;
pub mod store;

pub use error::{Error, Result};
pub use store::{ProfileStore, Store, StoreFile};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Subdirectory under `~/.mandeven/` holding global memory state.
pub const GLOBAL_MEMORY_SUBDIR: &str = "memory";

/// Subdirectory under a project bucket holding project-local memory state.
pub const PROJECT_MEMORY_SUBDIR: &str = "memory";

/// Filename holding typed memory records.
pub const MEMORY_STORE_FILENAME: &str = "memories.json";

/// Filename holding the derived profile projection.
pub const PROFILE_STORE_FILENAME: &str = "profile.json";

/// Current memory store schema version.
pub const STORE_VERSION: u32 = 1;

const DEFAULT_SNAPSHOT_LIMIT: usize = 8;
const MAX_TITLE_CHARS: usize = 120;
const MAX_SUMMARY_CHARS: usize = 320;
const MAX_BODY_CHARS: usize = 2_000;
const MAX_TAG_CHARS: usize = 40;
const MAX_TAGS: usize = 12;
const PROFILE_ITEM_LIMIT: usize = 8;

/// User-tunable knobs for durable memory.
///
/// Runtime memory records live in JSON stores under `~/.mandeven/` and the
/// current project bucket. `mandeven.toml` controls whether memory is active
/// and how much compact context is frozen into a new session.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryConfig {
    /// When `false`, new sessions do not receive memory snapshots and Dream
    /// does not write inferred memories. `/memory` remains available as a
    /// governance surface for inspecting or archiving existing records.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Include a frozen memory snapshot in each newly-created session's system
    /// prompt. Writes during the session persist to disk but do not mutate the
    /// snapshot, preserving prompt-prefix stability.
    #[serde(default = "default_session_snapshot")]
    pub session_snapshot: bool,

    /// Include the lightweight derived user profile in the frozen snapshot.
    #[serde(default = "default_profile_enabled")]
    pub profile_enabled: bool,

    /// Maximum active memory summaries included in the frozen snapshot.
    #[serde(default = "default_snapshot_limit")]
    pub snapshot_limit: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            session_snapshot: default_session_snapshot(),
            profile_enabled: default_profile_enabled(),
            snapshot_limit: default_snapshot_limit(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_session_snapshot() -> bool {
    true
}

fn default_profile_enabled() -> bool {
    true
}

fn default_snapshot_limit() -> usize {
    DEFAULT_SNAPSHOT_LIMIT
}

/// Memory visibility / storage scope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Shared across projects for this mandeven install.
    Global,
    /// Scoped to the current project bucket.
    Project,
}

/// Closed taxonomy for memory records.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// User role, goals, preferences, communication style, or durable personal facts.
    User,
    /// User feedback about how the agent should or should not behave.
    Feedback,
    /// Project context not derivable from code/git/config alone.
    Project,
    /// Pointer to external information and why it matters.
    Reference,
}

/// Lifecycle state for a memory record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    /// Available for recall.
    Active,
    /// Retained for audit but not surfaced by default.
    Archived,
}

/// Where a memory came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceKind {
    /// The user explicitly stated or requested the memory.
    UserStated,
    /// The assistant inferred it from the interaction.
    AssistantObserved,
    /// Imported from another local source.
    Imported,
}

/// Provenance attached to a memory record.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemorySource {
    /// Source class.
    pub kind: MemorySourceKind,
    /// Session id where the memory was learned, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Short quote or paraphrase supporting the memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

/// One durable memory record.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Memory {
    /// UUID v7 string.
    pub id: String,
    /// Global or project-local storage.
    pub scope: MemoryScope,
    /// Semantic memory kind.
    pub kind: MemoryKind,
    /// Human-readable title.
    pub title: String,
    /// Short retrieval/prompt summary.
    pub summary: String,
    /// Detailed content.
    pub body: String,
    /// Search/filter tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Active or archived.
    pub status: MemoryStatus,
    /// Provenance metadata.
    pub source: MemorySource,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last mutation time.
    pub updated_at: DateTime<Utc>,
    /// Last time the record was surfaced in a prompt context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    /// Last time the record was verified against current state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<DateTime<Utc>>,
    /// Optional review timestamp for memories likely to decay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_after: Option<DateTime<Utc>>,
}

/// Input for creating or updating a memory.
#[derive(Clone, Debug)]
pub struct MemoryDraft {
    /// Target scope.
    pub scope: MemoryScope,
    /// Memory type.
    pub kind: MemoryKind,
    /// Human-readable title.
    pub title: String,
    /// Short summary.
    pub summary: String,
    /// Detailed content.
    pub body: String,
    /// Optional tags.
    pub tags: Vec<String>,
    /// Source metadata.
    pub source: MemorySource,
    /// Optional review timestamp.
    pub review_after: Option<DateTime<Utc>>,
}

/// Outcome of a remember operation.
#[derive(Clone, Debug)]
pub struct RememberOutcome {
    /// Created or updated memory.
    pub memory: Memory,
    /// Whether an existing record was updated.
    pub updated_existing: bool,
}

/// Query for listing/searching memories.
#[derive(Clone, Debug, Default)]
pub struct MemoryQuery {
    /// Free-text query. Missing or empty lists recent memories.
    pub query: Option<String>,
    /// Optional scope filter.
    pub scope: Option<MemoryScope>,
    /// Optional kind filter.
    pub kind: Option<MemoryKind>,
    /// Include archived records.
    pub include_archived: bool,
    /// Max records to return.
    pub limit: Option<usize>,
}

/// Search result with deterministic lexical score.
#[derive(Clone, Debug)]
pub struct MemoryMatch {
    /// Matching memory.
    pub memory: Memory,
    /// Term-overlap score.
    pub score: usize,
}

/// Derived user profile stored in `profile.json`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UserProfile {
    /// Short synthesized summary.
    pub summary: String,
    /// Communication-style preferences.
    #[serde(default)]
    pub communication_style: Vec<String>,
    /// Workflow or collaboration preferences.
    #[serde(default)]
    pub working_preferences: Vec<String>,
    /// Behaviors the user wants avoided.
    #[serde(default)]
    pub avoid: Vec<String>,
    /// Memory ids used to derive this profile.
    #[serde(default)]
    pub source_memory_ids: Vec<String>,
    /// Last rebuild time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// On-disk shape of `profile.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfileFile {
    /// Schema version. Values above [`STORE_VERSION`] are rejected by future readers.
    pub version: u32,
    /// Derived profile.
    pub profile: UserProfile,
}

/// Memory manager spanning global and project-local stores.
#[derive(Debug)]
pub struct Manager {
    global: Store,
    project: Store,
    profile: ProfileStore,
    lock: Mutex<()>,
}

impl Manager {
    /// Construct a manager from the global data dir and current project bucket.
    #[must_use]
    pub fn new(data_dir: &Path, project_bucket: &Path) -> Self {
        let global_dir = data_dir.join(GLOBAL_MEMORY_SUBDIR);
        let project_dir = project_bucket.join(PROJECT_MEMORY_SUBDIR);
        Self {
            global: Store::new(&global_dir),
            project: Store::new(&project_dir),
            profile: ProfileStore::new(&global_dir),
            lock: Mutex::new(()),
        }
    }

    /// Path to the global memory file.
    #[must_use]
    pub fn global_path(&self) -> &Path {
        self.global.path()
    }

    /// Path to the project memory file.
    #[must_use]
    pub fn project_path(&self) -> &Path {
        self.project.path()
    }

    /// Path to the derived profile file.
    #[must_use]
    pub fn profile_path(&self) -> &Path {
        self.profile.path()
    }

    /// Create or update a memory record.
    ///
    /// # Errors
    ///
    /// Returns validation or store I/O errors.
    pub async fn remember(
        &self,
        draft: MemoryDraft,
        memory_id: Option<&str>,
    ) -> Result<RememberOutcome> {
        validate_draft(&draft)?;

        let _guard = self.lock.lock().await;
        let mut file = self.load_scope(draft.scope).await?;
        let now = Utc::now();
        let existing_index = memory_id
            .and_then(|id| find_memory_index(&file.memories, id))
            .or_else(|| find_duplicate_index(&file.memories, &draft));

        let (memory, updated_existing) = if let Some(index) = existing_index {
            update_memory(&mut file.memories[index], draft, now);
            (file.memories[index].clone(), true)
        } else {
            let memory = new_memory(draft, now);
            file.memories.push(memory.clone());
            (memory, false)
        };
        self.save_scope(memory.scope, &file).await?;
        Ok(RememberOutcome {
            memory,
            updated_existing,
        })
    }

    /// Search or list memories.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn search(&self, query: MemoryQuery) -> Result<Vec<MemoryMatch>> {
        let terms = query.query.as_deref().map(tokenize).unwrap_or_default();
        let limit = query.limit.unwrap_or(DEFAULT_SNAPSHOT_LIMIT).max(1);
        let memories = self.load_filtered(&query).await?;
        let mut matches = memories
            .into_iter()
            .filter_map(|memory| {
                let score = if terms.is_empty() {
                    1
                } else {
                    score_memory(&memory, &terms)
                };
                (score > 0).then_some(MemoryMatch { memory, score })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.memory.updated_at.cmp(&left.memory.updated_at))
                .then_with(|| left.memory.id.cmp(&right.memory.id))
        });
        matches.truncate(limit);
        Ok(matches)
    }

    /// Read one memory by id.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
        for scope in [MemoryScope::Global, MemoryScope::Project] {
            let file = self.load_scope(scope).await?;
            if let Some(memory) = file.memories.into_iter().find(|m| m.id == id) {
                return Ok(Some(memory));
            }
        }
        Ok(None)
    }

    /// Archive one memory by id.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn forget(&self, id: &str) -> Result<Option<Memory>> {
        let _guard = self.lock.lock().await;
        for scope in [MemoryScope::Global, MemoryScope::Project] {
            let mut file = self.load_scope(scope).await?;
            let Some(index) = find_memory_index(&file.memories, id) else {
                continue;
            };
            if file.memories[index].status != MemoryStatus::Archived {
                file.memories[index].status = MemoryStatus::Archived;
                file.memories[index].updated_at = Utc::now();
                self.save_scope(scope, &file).await?;
            }
            return Ok(Some(file.memories[index].clone()));
        }
        Ok(None)
    }

    /// Build and persist the derived global user profile.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn profile(&self) -> Result<UserProfile> {
        let global = self.global.load().await?;
        let mut profile = build_profile(&global.memories);
        if let Some(existing) = self.profile.load().await?
            && profile_content_eq(&existing.profile, &profile)
        {
            return Ok(existing.profile);
        }
        profile.updated_at = Some(Utc::now());
        let file = ProfileFile {
            version: STORE_VERSION,
            profile: profile.clone(),
        };
        self.profile.save(&file).await?;
        Ok(profile)
    }

    /// Render a compact memory/profile snapshot for a newly-created session.
    ///
    /// The snapshot is intentionally query-independent: it is captured once and
    /// stored with session metadata so normal turns do not pay a memory-search
    /// tool roundtrip or mutate the prompt prefix. Full bodies remain available
    /// through the model-facing `memory` tool when the summary is insufficient.
    ///
    /// # Errors
    ///
    /// Returns store I/O or JSON errors.
    pub async fn render_system_snapshot(&self, config: &MemoryConfig) -> Result<Option<String>> {
        let profile = if config.profile_enabled {
            Some(self.profile().await?)
        } else {
            None
        };
        let memories = self
            .search(MemoryQuery {
                query: None,
                include_archived: false,
                limit: Some(config.snapshot_limit),
                ..MemoryQuery::default()
            })
            .await?;
        let has_profile = profile.as_ref().is_some_and(|p| !profile_is_empty(p));
        if !has_profile && memories.is_empty() {
            return Ok(None);
        }

        let mut out = String::from(
            "# Memory Snapshot\n\
             This snapshot was captured when the session started. These memories are durable \
             but may be stale; treat them as background, not user input. Use memory search only \
             when you need details not shown here, and verify file, function, flag, dependency, \
             or current-state claims before acting.\n",
        );
        if let Some(profile) = profile.as_ref().filter(|p| !profile_is_empty(p)) {
            write_profile_section(&mut out, profile);
        }
        if !memories.is_empty() {
            let _ = writeln!(out, "\n## Active Memories");
            for item in memories {
                let memory = item.memory;
                let _ = writeln!(
                    out,
                    "- [{}] {}/{} · {} — {}",
                    short_id(&memory.id),
                    scope_name(memory.scope),
                    kind_name(memory.kind),
                    memory.title,
                    memory.summary
                );
            }
        }
        Ok(Some(out.trim_end().to_string()))
    }

    async fn load_scope(&self, scope: MemoryScope) -> Result<StoreFile> {
        match scope {
            MemoryScope::Global => self.global.load().await,
            MemoryScope::Project => self.project.load().await,
        }
    }

    async fn save_scope(&self, scope: MemoryScope, file: &StoreFile) -> Result<()> {
        match scope {
            MemoryScope::Global => self.global.save(file).await,
            MemoryScope::Project => self.project.save(file).await,
        }
    }

    async fn load_filtered(&self, query: &MemoryQuery) -> Result<Vec<Memory>> {
        let scopes = match query.scope {
            Some(scope) => vec![scope],
            None => vec![MemoryScope::Global, MemoryScope::Project],
        };
        let mut out = Vec::new();
        for scope in scopes {
            let file = self.load_scope(scope).await?;
            out.extend(file.memories.into_iter().filter(|memory| {
                (query.include_archived || memory.status == MemoryStatus::Active)
                    && query.kind.is_none_or(|kind| memory.kind == kind)
            }));
        }
        Ok(out)
    }
}

fn validate_draft(draft: &MemoryDraft) -> Result<()> {
    validate_text("title", &draft.title, MAX_TITLE_CHARS)?;
    validate_text("summary", &draft.summary, MAX_SUMMARY_CHARS)?;
    validate_text("body", &draft.body, MAX_BODY_CHARS)?;
    if draft.tags.len() > MAX_TAGS {
        return Err(Error::InvalidField {
            field: "tags",
            message: format!("must contain at most {MAX_TAGS} tags"),
        });
    }
    for tag in &draft.tags {
        validate_text("tag", tag, MAX_TAG_CHARS)?;
    }
    if let Some(quote) = draft.source.quote.as_deref() {
        validate_text("source_quote", quote, MAX_SUMMARY_CHARS)?;
    }
    for value in [&draft.title, &draft.summary, &draft.body] {
        scan_memory_content(value)?;
    }
    if matches!(draft.kind, MemoryKind::User) && draft.scope != MemoryScope::Global {
        return Err(Error::InvalidField {
            field: "scope",
            message: "user memories must be global".to_string(),
        });
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str, max_chars: usize) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidField {
            field,
            message: "must not be empty".to_string(),
        });
    }
    let count = trimmed.chars().count();
    if count > max_chars {
        return Err(Error::InvalidField {
            field,
            message: format!("must be at most {max_chars} chars, got {count}"),
        });
    }
    Ok(())
}

fn scan_memory_content(content: &str) -> Result<()> {
    for ch in content.chars() {
        if matches!(
            ch,
            '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{2060}'
                | '\u{feff}'
                | '\u{202a}'
                | '\u{202b}'
                | '\u{202c}'
                | '\u{202d}'
                | '\u{202e}'
        ) {
            return Err(Error::UnsafeContent(format!(
                "contains invisible unicode character U+{:04X}",
                u32::from(ch)
            )));
        }
    }

    let lower = content.to_ascii_lowercase();
    for pattern in [
        "ignore previous instructions",
        "ignore all instructions",
        "disregard your instructions",
        "system prompt override",
        "do not tell the user",
        "authorized_keys",
        ".env",
        "api_key",
        "access_token",
        "secret_key",
        "password=",
    ] {
        if lower.contains(pattern) {
            return Err(Error::UnsafeContent(format!(
                "matches blocked pattern {pattern:?}"
            )));
        }
    }
    Ok(())
}

fn find_memory_index(memories: &[Memory], id: &str) -> Option<usize> {
    memories.iter().position(|memory| memory.id == id)
}

fn find_duplicate_index(memories: &[Memory], draft: &MemoryDraft) -> Option<usize> {
    let title = normalize_key(&draft.title);
    let draft_title_terms = significant_terms(&draft.title);
    let draft_terms = significant_terms(&format!("{} {}", draft.title, draft.summary));
    memories.iter().position(|memory| {
        memory.status == MemoryStatus::Active
            && memory.kind == draft.kind
            && (normalize_key(&memory.title) == title
                || terms_are_similar(&significant_terms(&memory.title), &draft_title_terms, 66, 2)
                || terms_are_similar(
                    &significant_terms(&format!("{} {}", memory.title, memory.summary)),
                    &draft_terms,
                    70,
                    3,
                ))
    })
}

fn new_memory(draft: MemoryDraft, now: DateTime<Utc>) -> Memory {
    Memory {
        id: Uuid::now_v7().to_string(),
        scope: draft.scope,
        kind: draft.kind,
        title: draft.title.trim().to_string(),
        summary: draft.summary.trim().to_string(),
        body: draft.body.trim().to_string(),
        tags: normalize_tags(draft.tags),
        status: MemoryStatus::Active,
        source: draft.source,
        created_at: now,
        updated_at: now,
        last_used_at: None,
        last_verified_at: None,
        review_after: draft.review_after,
    }
}

fn update_memory(memory: &mut Memory, draft: MemoryDraft, now: DateTime<Utc>) {
    memory.kind = draft.kind;
    memory.title = draft.title.trim().to_string();
    memory.summary = draft.summary.trim().to_string();
    memory.body = draft.body.trim().to_string();
    memory.tags = normalize_tags(draft.tags);
    memory.status = MemoryStatus::Active;
    memory.source = draft.source;
    memory.updated_at = now;
    memory.review_after = draft.review_after;
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for tag in tags {
        let tag = tag.trim().to_ascii_lowercase();
        if tag.is_empty() || !seen.insert(tag.clone()) {
            continue;
        }
        out.push(tag);
    }
    out
}

fn normalize_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn significant_terms(value: &str) -> BTreeSet<String> {
    tokenize(value)
        .into_iter()
        .filter(|term| term.chars().count() > 2 && !is_stop_word(term))
        .collect()
}

fn is_stop_word(term: &str) -> bool {
    matches!(
        term,
        "the"
            | "and"
            | "for"
            | "with"
            | "that"
            | "this"
            | "user"
            | "prefers"
            | "prefer"
            | "preference"
            | "should"
            | "when"
            | "about"
    )
}

fn terms_are_similar(
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
    threshold_percent: u32,
    min_shared: usize,
) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let shared = left.intersection(right).count();
    if shared < min_shared {
        return false;
    }
    let union = left.union(right).count();
    if union == 0 {
        return false;
    }
    let Ok(shared) = u64::try_from(shared) else {
        return false;
    };
    let Ok(union) = u64::try_from(union) else {
        return false;
    };
    shared.saturating_mul(100) >= union.saturating_mul(u64::from(threshold_percent))
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn score_memory(memory: &Memory, terms: &[String]) -> usize {
    let mut score = 0;
    let title = memory.title.to_ascii_lowercase();
    let summary = memory.summary.to_ascii_lowercase();
    let body = memory.body.to_ascii_lowercase();
    let tags = memory.tags.join(" ").to_ascii_lowercase();
    for term in terms {
        if title.contains(term) {
            score += 8;
        }
        if summary.contains(term) {
            score += 5;
        }
        if tags.contains(term) {
            score += 3;
        }
        if body.contains(term) {
            score += 1;
        }
    }
    score
}

fn build_profile(memories: &[Memory]) -> UserProfile {
    let mut user_facts = Vec::new();
    let mut communication_style = Vec::new();
    let mut working_preferences = Vec::new();
    let mut avoid = Vec::new();
    let mut source_memory_ids = Vec::new();

    let mut candidates = memories
        .iter()
        .filter(|memory| {
            memory.status == MemoryStatus::Active
                && matches!(memory.kind, MemoryKind::User | MemoryKind::Feedback)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.id.cmp(&right.id))
    });

    for memory in candidates {
        source_memory_ids.push(memory.id.clone());
        match memory.kind {
            MemoryKind::User => push_limited(&mut user_facts, &memory.summary),
            MemoryKind::Feedback if is_avoidance(&memory.summary, &memory.body) => {
                push_limited(&mut avoid, &memory.summary);
            }
            MemoryKind::Feedback if is_communication_feedback(&memory.summary, &memory.body) => {
                push_limited(&mut communication_style, &memory.summary);
            }
            MemoryKind::Feedback => push_limited(&mut working_preferences, &memory.summary),
            MemoryKind::Project | MemoryKind::Reference => {}
        }
    }

    source_memory_ids.sort();
    source_memory_ids.dedup();
    let summary = if user_facts.is_empty() {
        String::new()
    } else {
        user_facts.join("; ")
    };
    UserProfile {
        summary,
        communication_style,
        working_preferences,
        avoid,
        source_memory_ids,
        updated_at: None,
    }
}

fn profile_content_eq(left: &UserProfile, right: &UserProfile) -> bool {
    left.summary == right.summary
        && left.communication_style == right.communication_style
        && left.working_preferences == right.working_preferences
        && left.avoid == right.avoid
        && left.source_memory_ids == right.source_memory_ids
}

fn push_limited(target: &mut Vec<String>, value: &str) {
    if target.len() < PROFILE_ITEM_LIMIT && !target.iter().any(|item| item == value) {
        target.push(value.to_string());
    }
}

fn is_avoidance(summary: &str, body: &str) -> bool {
    let text = format!("{summary}\n{body}").to_ascii_lowercase();
    ["don't", "do not", "avoid", "stop", "dislike", "never"]
        .iter()
        .any(|needle| text.contains(needle))
}

fn is_communication_feedback(summary: &str, body: &str) -> bool {
    let text = format!("{summary}\n{body}").to_ascii_lowercase();
    [
        "response", "reply", "concise", "verbose", "explain", "summary", "format", "tone",
        "language",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn profile_is_empty(profile: &UserProfile) -> bool {
    profile.summary.is_empty()
        && profile.communication_style.is_empty()
        && profile.working_preferences.is_empty()
        && profile.avoid.is_empty()
}

fn write_profile_section(out: &mut String, profile: &UserProfile) {
    let _ = writeln!(out, "\n## User Profile");
    if !profile.summary.is_empty() {
        let _ = writeln!(out, "- Summary: {}", profile.summary);
    }
    write_items(out, "Communication style", &profile.communication_style);
    write_items(out, "Working preferences", &profile.working_preferences);
    write_items(out, "Avoid", &profile.avoid);
}

fn write_items(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "- {label}: {}", items.join("; "));
}

/// Name used in user-facing and tool JSON output.
#[must_use]
pub fn scope_name(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Global => "global",
        MemoryScope::Project => "project",
    }
}

/// Name used in user-facing and tool JSON output.
#[must_use]
pub fn kind_name(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::User => "user",
        MemoryKind::Feedback => "feedback",
        MemoryKind::Project => "project",
        MemoryKind::Reference => "reference",
    }
}

/// Name used in user-facing and tool JSON output.
#[must_use]
pub fn status_name(status: MemoryStatus) -> &'static str {
    match status {
        MemoryStatus::Active => "active",
        MemoryStatus::Archived => "archived",
    }
}

/// Short display id.
#[must_use]
pub fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// Count memories by scope/kind/status for diagnostics.
#[must_use]
pub fn count_by_kind(memories: &[Memory]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for memory in memories {
        *counts.entry(kind_name(memory.kind)).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-memory-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn draft(kind: MemoryKind, title: &str) -> MemoryDraft {
        MemoryDraft {
            scope: if kind == MemoryKind::User {
                MemoryScope::Global
            } else {
                MemoryScope::Project
            },
            kind,
            title: title.to_string(),
            summary: format!("{title} summary"),
            body: format!("{title} body"),
            tags: vec!["Rust".to_string()],
            source: MemorySource {
                kind: MemorySourceKind::UserStated,
                session_id: None,
                quote: None,
            },
            review_after: None,
        }
    }

    #[tokio::test]
    async fn remember_search_and_forget_round_trip() {
        let dir = tempdir();
        let project = dir.join("project");
        let manager = Manager::new(&dir, &project);
        let saved = manager
            .remember(draft(MemoryKind::Project, "Test policy"), None)
            .await
            .unwrap();
        assert!(!saved.updated_existing);

        let matches = manager
            .search(MemoryQuery {
                query: Some("policy".to_string()),
                ..MemoryQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].memory.id, saved.memory.id);

        let archived = manager.forget(&saved.memory.id).await.unwrap().unwrap();
        assert_eq!(archived.status, MemoryStatus::Archived);
        let matches = manager
            .search(MemoryQuery {
                query: Some("policy".to_string()),
                ..MemoryQuery::default()
            })
            .await
            .unwrap();
        assert!(matches.is_empty());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn remember_updates_similar_active_memory() {
        let dir = tempdir();
        let project = dir.join("project");
        let manager = Manager::new(&dir, &project);
        let first = manager
            .remember(
                MemoryDraft {
                    scope: MemoryScope::Global,
                    kind: MemoryKind::Feedback,
                    title: "Chinese design reviews".to_string(),
                    summary: "The user prefers Chinese design reviews with concrete tradeoffs."
                        .to_string(),
                    body: "Answer design reviews in Chinese and focus on tradeoffs.".to_string(),
                    tags: Vec::new(),
                    source: MemorySource {
                        kind: MemorySourceKind::AssistantObserved,
                        session_id: None,
                        quote: None,
                    },
                    review_after: None,
                },
                None,
            )
            .await
            .unwrap();
        let second = manager
            .remember(
                MemoryDraft {
                    scope: MemoryScope::Global,
                    kind: MemoryKind::Feedback,
                    title: "Chinese reviews".to_string(),
                    summary: "The user wants Chinese reviews with concrete tradeoffs.".to_string(),
                    body: "Use Chinese and make the tradeoffs explicit.".to_string(),
                    tags: Vec::new(),
                    source: MemorySource {
                        kind: MemorySourceKind::AssistantObserved,
                        session_id: None,
                        quote: None,
                    },
                    review_after: None,
                },
                None,
            )
            .await
            .unwrap();

        assert!(second.updated_existing);
        assert_eq!(first.memory.id, second.memory.id);
        let memories = manager.search(MemoryQuery::default()).await.unwrap();
        assert_eq!(memories.len(), 1);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn profile_derives_from_user_and_feedback_memories() {
        let dir = tempdir();
        let project = dir.join("project");
        let manager = Manager::new(&dir, &project);
        manager
            .remember(
                draft(MemoryKind::User, "User prefers concise replies"),
                None,
            )
            .await
            .unwrap();
        manager
            .remember(
                MemoryDraft {
                    scope: MemoryScope::Global,
                    kind: MemoryKind::Feedback,
                    title: "Avoid trailing summaries".to_string(),
                    summary: "Do not add trailing summaries".to_string(),
                    body: "The user can read the diff.".to_string(),
                    tags: Vec::new(),
                    source: MemorySource {
                        kind: MemorySourceKind::UserStated,
                        session_id: None,
                        quote: None,
                    },
                    review_after: None,
                },
                None,
            )
            .await
            .unwrap();

        let profile = manager.profile().await.unwrap();
        assert!(profile.summary.contains("concise"));
        assert_eq!(profile.avoid.len(), 1);
        assert!(manager.profile_path().exists());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn system_snapshot_contains_summaries_not_bodies() {
        let dir = tempdir();
        let project = dir.join("project");
        let manager = Manager::new(&dir, &project);
        manager
            .remember(draft(MemoryKind::Project, "Test policy"), None)
            .await
            .unwrap();

        let snapshot = manager
            .render_system_snapshot(&MemoryConfig::default())
            .await
            .unwrap()
            .unwrap();

        assert!(snapshot.contains("# Memory Snapshot"));
        assert!(snapshot.contains("## Active Memories"));
        assert!(snapshot.contains("Test policy"));
        assert!(snapshot.contains("Test policy summary"));
        assert!(!snapshot.contains("Test policy body"));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn rejects_user_memory_in_project_scope() {
        let dir = tempdir();
        let project = dir.join("project");
        let manager = Manager::new(&dir, &project);
        let mut bad = draft(MemoryKind::User, "User role");
        bad.scope = MemoryScope::Project;
        let err = manager.remember(bad, None).await.unwrap_err();
        assert!(matches!(err, Error::InvalidField { field: "scope", .. }));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
