//! Model-facing memory tool.
//!
//! A single `memory` tool keeps the schema surface small while still exposing
//! the three operations the model needs: remember, search/list, and forget.
//! User-facing inspection/removal lives in `/memory`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use schemars::schema_for_value;
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::{Error, Result};
use super::{BaseTool, Registry, ToolOutcome};
use crate::llm::Tool;
use crate::memory::{
    self, MemoryDraft, MemoryKind, MemoryQuery, MemoryScope, MemorySource, MemorySourceKind,
    UserProfile,
};

/// Register the model-facing memory tool.
pub fn register(registry: &mut Registry, memory: Arc<memory::Manager>) {
    registry.register(Arc::new(MemoryTool { memory }));
}

/// Single memory tool.
pub struct MemoryTool {
    memory: Arc<memory::Manager>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum MemoryParams {
    /// Create or update durable memory.
    Remember {
        /// Update this existing memory id. Omit to create or upsert by title/kind/scope.
        #[serde(default)]
        memory_id: Option<String>,
        /// Target scope. Use global for user/profile-level facts; project for current project context.
        scope: ScopeParam,
        /// Memory type.
        kind: KindParam,
        /// Short human-readable title.
        title: String,
        /// One-sentence summary used for recall and prompt injection.
        summary: String,
        /// Detailed durable content.
        body: String,
        /// Optional tags.
        #[serde(default)]
        tags: Vec<String>,
        /// Source kind. Defaults to `user_stated`.
        #[serde(default)]
        source_kind: Option<SourceKindParam>,
        /// Short quote or paraphrase that justifies saving this memory.
        #[serde(default)]
        source_quote: Option<String>,
        /// Optional RFC3339 review timestamp for memories likely to decay.
        #[serde(default)]
        review_after: Option<String>,
    },
    /// Search memories only when the frozen session snapshot lacks needed
    /// details; omit query to list recent active records.
    Search {
        /// Free-text query.
        #[serde(default)]
        query: Option<String>,
        /// Optional scope filter.
        #[serde(default)]
        scope: Option<ScopeParam>,
        /// Optional type filter.
        #[serde(default)]
        kind: Option<KindParam>,
        /// Include archived memories.
        #[serde(default)]
        include_archived: Option<bool>,
        /// Maximum number of memories to return.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Archive a memory so it is no longer recalled by default.
    Forget {
        /// Memory id.
        memory_id: String,
    },
    /// Read the lightweight derived user profile.
    Profile,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScopeParam {
    Global,
    Project,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KindParam {
    User,
    Feedback,
    Project,
    Reference,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceKindParam {
    UserStated,
    AssistantObserved,
    Imported,
}

impl From<ScopeParam> for MemoryScope {
    fn from(value: ScopeParam) -> Self {
        match value {
            ScopeParam::Global => Self::Global,
            ScopeParam::Project => Self::Project,
        }
    }
}

impl From<KindParam> for MemoryKind {
    fn from(value: KindParam) -> Self {
        match value {
            KindParam::User => Self::User,
            KindParam::Feedback => Self::Feedback,
            KindParam::Project => Self::Project,
            KindParam::Reference => Self::Reference,
        }
    }
}

impl From<SourceKindParam> for MemorySourceKind {
    fn from(value: SourceKindParam) -> Self {
        match value {
            SourceKindParam::UserStated => Self::UserStated,
            SourceKindParam::AssistantObserved => Self::AssistantObserved,
            SourceKindParam::Imported => Self::Imported,
        }
    }
}

struct RememberArgs {
    memory_id: Option<String>,
    scope: ScopeParam,
    kind: KindParam,
    title: String,
    summary: String,
    body: String,
    tags: Vec<String>,
    source_kind: Option<SourceKindParam>,
    source_quote: Option<String>,
    review_after: Option<String>,
}

struct SearchArgs {
    query: Option<String>,
    scope: Option<ScopeParam>,
    kind: Option<KindParam>,
    include_archived: Option<bool>,
    limit: Option<usize>,
}

#[async_trait]
impl BaseTool for MemoryTool {
    fn schema(&self) -> Tool {
        Tool {
            name: "memory".into(),
            description: "Manage durable memory that survives across sessions. \
                Use proactively when the user gives stable preferences, corrections, \
                project context not derivable from code/git, or external reference pointers. \
                Do not save task progress, cron run state, raw logs, secrets, large code/data, \
                or facts already documented in AGENTS.md. Memory is model-facing state; \
                /memory is the user-facing governance surface. A compact memory snapshot is \
                already included in the session context when available; use action=search only \
                when you need full body/details not visible in that snapshot. Use action=profile \
                to inspect the simple derived user profile."
                .into(),
            parameters: memory_parameters_schema(),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: MemoryParams = parse_params("memory", args)?;
        match params {
            MemoryParams::Remember {
                memory_id,
                scope,
                kind,
                title,
                summary,
                body,
                tags,
                source_kind,
                source_quote,
                review_after,
            } => {
                self.call_remember(RememberArgs {
                    memory_id,
                    scope,
                    kind,
                    title,
                    summary,
                    body,
                    tags,
                    source_kind,
                    source_quote,
                    review_after,
                })
                .await
            }
            MemoryParams::Search {
                query,
                scope,
                kind,
                include_archived,
                limit,
            } => {
                self.call_search(SearchArgs {
                    query,
                    scope,
                    kind,
                    include_archived,
                    limit,
                })
                .await
            }
            MemoryParams::Forget { memory_id } => self.call_forget(memory_id).await,
            MemoryParams::Profile => self.call_profile().await,
        }
    }
}

fn memory_parameters_schema() -> Value {
    let mut schema = serde_json::to_value(schema_for_value!(json!({
            "action": "remember",
            "memory_id": "mem_0123456789abcdef",
            "scope": "global",
            "kind": "user",
            "title": "User response preference",
            "summary": "User prefers concise replies",
            "body": "Keep responses direct unless the user asks for detail.",
            "tags": ["style"],
            "source_kind": "user_stated",
            "source_quote": "concise please",
            "review_after": "2026-01-01T00:00:00Z",
            "query": "concise",
            "include_archived": false,
            "limit": 8,
    })))
    .expect("schema_for_value output always serializes");

    if let Some(obj) = schema.as_object_mut() {
        obj.insert("required".to_string(), json!(["action"]));
        obj.insert("additionalProperties".to_string(), json!(false));
        if let Some(action) = obj
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|properties| properties.get_mut("action"))
            .and_then(Value::as_object_mut)
        {
            action.insert(
                "enum".to_string(),
                json!(["remember", "search", "forget", "profile"]),
            );
        }
    }

    schema
}

impl MemoryTool {
    async fn call_remember(&self, args: RememberArgs) -> Result<ToolOutcome> {
        let draft = MemoryDraft {
            scope: args.scope.into(),
            kind: args.kind.into(),
            title: args.title,
            summary: args.summary,
            body: args.body,
            tags: args.tags,
            source: MemorySource {
                kind: args
                    .source_kind
                    .unwrap_or(SourceKindParam::UserStated)
                    .into(),
                session_id: None,
                quote: args.source_quote,
            },
            review_after: parse_optional_utc("review_after", args.review_after.as_deref())?,
        };
        let outcome = self
            .memory
            .remember(draft, args.memory_id.as_deref())
            .await
            .map_err(|err| exec("memory", &err))?;
        Ok(json!({
            "success": true,
            "updated_existing": outcome.updated_existing,
            "memory": memory_detail(&outcome.memory),
            "message": if outcome.updated_existing { "Memory updated" } else { "Memory saved" },
        })
        .into())
    }

    async fn call_search(&self, args: SearchArgs) -> Result<ToolOutcome> {
        let matches = self
            .memory
            .search(MemoryQuery {
                query: args.query,
                scope: args.scope.map(Into::into),
                kind: args.kind.map(Into::into),
                include_archived: args.include_archived.unwrap_or(false),
                limit: args.limit,
            })
            .await
            .map_err(|err| exec("memory", &err))?;
        let memories: Vec<Value> = matches
            .iter()
            .map(|item| {
                let mut value = memory_summary(&item.memory);
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("score".to_string(), json!(item.score));
                }
                value
            })
            .collect();
        let count = memories.len();
        Ok(json!({
            "memories": memories,
            "count": count,
        })
        .into())
    }

    async fn call_forget(&self, memory_id: String) -> Result<ToolOutcome> {
        let memory = self
            .memory
            .forget(&memory_id)
            .await
            .map_err(|err| exec("memory", &err))?;
        Ok(json!({
            "success": memory.is_some(),
            "memory_id": memory_id,
            "memory": memory.as_ref().map(memory_summary),
            "message": if memory.is_some() { "Memory archived" } else { "Memory not found" },
        })
        .into())
    }

    async fn call_profile(&self) -> Result<ToolOutcome> {
        let profile = self
            .memory
            .profile()
            .await
            .map_err(|err| exec("memory", &err))?;
        Ok(json!({
            "profile": profile_json(&profile),
        })
        .into())
    }
}

fn memory_summary(memory: &memory::Memory) -> Value {
    json!({
        "id": &memory.id,
        "short_id": memory::short_id(&memory.id),
        "scope": memory::scope_name(memory.scope),
        "kind": memory::kind_name(memory.kind),
        "status": memory::status_name(memory.status),
        "title": &memory.title,
        "summary": &memory.summary,
        "tags": &memory.tags,
        "updated_at": memory.updated_at.to_rfc3339(),
    })
}

fn memory_detail(memory: &memory::Memory) -> Value {
    json!({
        "id": &memory.id,
        "short_id": memory::short_id(&memory.id),
        "scope": memory::scope_name(memory.scope),
        "kind": memory::kind_name(memory.kind),
        "status": memory::status_name(memory.status),
        "title": &memory.title,
        "summary": &memory.summary,
        "body": &memory.body,
        "tags": &memory.tags,
        "source": &memory.source,
        "created_at": memory.created_at.to_rfc3339(),
        "updated_at": memory.updated_at.to_rfc3339(),
        "last_used_at": memory.last_used_at.map(|time| time.to_rfc3339()),
        "last_verified_at": memory.last_verified_at.map(|time| time.to_rfc3339()),
        "review_after": memory.review_after.map(|time| time.to_rfc3339()),
    })
}

fn profile_json(profile: &UserProfile) -> Value {
    json!({
        "summary": &profile.summary,
        "communication_style": &profile.communication_style,
        "working_preferences": &profile.working_preferences,
        "avoid": &profile.avoid,
        "source_memory_ids": &profile.source_memory_ids,
        "updated_at": profile.updated_at.map(|time| time.to_rfc3339()),
    })
}

fn parse_optional_utc(field: &'static str, value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|raw| {
            DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|err| exec("memory", &format_args!("invalid {field}: {err}")))
        })
        .transpose()
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
        let base = std::env::temp_dir().join(format!(
            "mandeven-memory-tool-test-{}",
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test]
    async fn memory_tool_remembers_searches_and_forgets() {
        let dir = tempdir();
        let manager = Arc::new(memory::Manager::new(&dir, &dir.join("project")));
        let tool = MemoryTool {
            memory: manager.clone(),
        };

        let result = tool
            .call(json!({
                "action": "remember",
                "scope": "global",
                "kind": "user",
                "title": "User response preference",
                "summary": "User prefers concise replies",
                "body": "Keep responses direct unless the user asks for detail.",
                "tags": ["style"],
                "source_quote": "concise please"
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("memory remember should return plain result");
        };
        let id = value["memory"]["id"].as_str().unwrap().to_string();

        let result = tool
            .call(json!({
                "action": "search",
                "query": "concise"
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("memory search should return plain result");
        };
        assert_eq!(value["count"], 1);

        tool.call(json!({
            "action": "forget",
            "memory_id": id
        }))
        .await
        .unwrap();
        let result = tool.call(json!({ "action": "search" })).await.unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("memory search should return plain result");
        };
        assert_eq!(value["count"], 0);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[test]
    fn memory_tool_schema_is_top_level_object() {
        let dir = tempdir();
        let tool = MemoryTool {
            memory: Arc::new(memory::Manager::new(&dir, &dir.join("project"))),
        };
        let schema = tool.schema().parameters;

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(schema["properties"]["action"]["type"], "string");
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["remember", "search", "forget", "profile"])
        );
        assert_eq!(schema["properties"]["memory_id"]["type"], "string");
        assert!(!schema_keyword_exists(&schema, "oneOf"));
        assert!(!schema_keyword_exists(&schema, "anyOf"));
        assert!(!schema_keyword_exists(&schema, "allOf"));
        assert!(!schema_type_contains_null(&schema));

        std::fs::remove_dir_all(dir).unwrap();
    }

    fn schema_keyword_exists(value: &Value, keyword: &str) -> bool {
        match value {
            Value::Object(map) => map
                .iter()
                .any(|(key, value)| key == keyword || schema_keyword_exists(value, keyword)),
            Value::Array(values) => values
                .iter()
                .any(|value| schema_keyword_exists(value, keyword)),
            _ => false,
        }
    }

    fn schema_type_contains_null(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                if let Some(ty) = map.get("type") {
                    match ty {
                        Value::String(s) if s == "null" => return true,
                        Value::Array(values) if values.iter().any(|v| v == "null") => return true,
                        _ => {}
                    }
                }
                map.values().any(schema_type_contains_null)
            }
            Value::Array(values) => values.iter().any(schema_type_contains_null),
            _ => false,
        }
    }
}
