//! Optional model-facing memory tools.
//!
//! Mirrors Claude Code's tool-shape preference: each durable-memory
//! operation is a dedicated tool with its own strict object schema.
//! The default runtime does not register this module: Dream owns inferred
//! memory writes, and user-facing inspection/removal still lives in `/memory`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::Result;
use super::schema;
use super::{BaseTool, Registry, ToolOutcome, exec_error, parse_params};
use crate::llm::Tool;
use crate::memory::{
    self, MemoryDraft, MemoryKind, MemoryQuery, MemoryScope, MemorySource, MemorySourceKind,
    UserProfile,
};

/// Register the optional model-facing memory tools.
pub fn register(registry: &mut Registry, memory: Arc<memory::Manager>) {
    registry.register(Arc::new(MemoryRemember {
        memory: memory.clone(),
    }));
    registry.register(Arc::new(MemorySearch {
        memory: memory.clone(),
    }));
    registry.register(Arc::new(MemoryForget {
        memory: memory.clone(),
    }));
    registry.register(Arc::new(MemoryProfile { memory }));
}

struct MemoryRemember {
    memory: Arc<memory::Manager>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryRememberParams {
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
}

struct MemorySearch {
    memory: Arc<memory::Manager>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemorySearchParams {
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
}

struct MemoryForget {
    memory: Arc<memory::Manager>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryForgetParams {
    /// Memory id.
    memory_id: String,
}

struct MemoryProfile {
    memory: Arc<memory::Manager>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryProfileParams {}

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

#[async_trait]
impl BaseTool for MemoryRemember {
    fn schema(&self) -> Tool {
        Tool {
            name: "memory_remember".into(),
            description: "Save or update durable memory that should survive across \
                sessions. Use only for stable user preferences, corrections, project \
                context not derivable from code/git, external reference pointers, or \
                durable feedback. Do not save task progress, cron state, raw logs, \
                secrets, large code/data, facts already documented in AGENTS.md, or \
                current conversation state. /memory is the user-facing governance surface."
                .into(),
            parameters: remember_parameters_schema(),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        remember(&self.memory, parse_params("memory_remember", args)?).await
    }
}

#[async_trait]
impl BaseTool for MemorySearch {
    fn schema(&self) -> Tool {
        Tool {
            name: "memory_search".into(),
            description: "Search durable memory only when the compact memory snapshot \
                lacks needed body/details. Omit query to list recent active records. \
                Memory is model-facing state; /memory is the user-facing governance surface."
                .into(),
            parameters: search_parameters_schema(),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        search(&self.memory, parse_params("memory_search", args)?).await
    }
}

#[async_trait]
impl BaseTool for MemoryForget {
    fn schema(&self) -> Tool {
        Tool {
            name: "memory_forget".into(),
            description: "Archive a durable memory by id so it is no longer recalled \
                by default. Use only when the user asks to forget/remove a memory or \
                when a saved memory is clearly obsolete or wrong."
                .into(),
            parameters: forget_parameters_schema(),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let params: MemoryForgetParams = parse_params("memory_forget", args)?;
        forget(&self.memory, params.memory_id).await
    }
}

#[async_trait]
impl BaseTool for MemoryProfile {
    fn schema(&self) -> Tool {
        Tool {
            name: "memory_profile".into(),
            description: "Read the lightweight derived user profile built from durable \
                memories. Use only when the compact memory snapshot is insufficient."
                .into(),
            parameters: schema::empty_object(),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let _: MemoryProfileParams = parse_params("memory_profile", args)?;
        profile(&self.memory).await
    }
}

fn remember_parameters_schema() -> Value {
    let mut schema = schema::object_from_example(
        &json!({
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
        }),
        &["scope", "kind", "title", "summary", "body"],
    );

    describe_memory_common(&mut schema);
    schema::describe(
        &mut schema,
        "memory_id",
        "Existing memory id to update. Omit to create or upsert by title/kind/scope.",
    );
    schema::describe(
        &mut schema,
        "title",
        "Short human-readable title for the durable memory.",
    );
    schema::describe(
        &mut schema,
        "summary",
        "One-sentence summary used for compact recall and prompt injection.",
    );
    schema::describe(
        &mut schema,
        "body",
        "Detailed durable content. Keep it concise; do not store secrets, logs, or large data.",
    );
    schema::describe(
        &mut schema,
        "tags",
        "Optional short tags for grouping memories.",
    );
    schema::describe(
        &mut schema,
        "source_kind",
        "Where the memory came from. Defaults to user_stated.",
    );
    schema::enum_strings(
        &mut schema,
        "source_kind",
        &["user_stated", "assistant_observed", "imported"],
    );
    schema::describe(
        &mut schema,
        "source_quote",
        "Short quote or paraphrase that justifies saving this memory.",
    );
    schema::describe(
        &mut schema,
        "review_after",
        "Optional RFC3339 timestamp for memories likely to decay.",
    );

    schema
}

fn search_parameters_schema() -> Value {
    let mut schema = schema::object_from_example(
        &json!({
            "query": "concise",
            "scope": "global",
            "kind": "user",
            "include_archived": false,
            "limit": 8,
        }),
        &[],
    );

    describe_memory_common(&mut schema);
    schema::describe(
        &mut schema,
        "query",
        "Free-text search query. Omit to list recent active records.",
    );
    schema::describe(
        &mut schema,
        "include_archived",
        "Include archived memories. Defaults to false.",
    );
    schema::describe(
        &mut schema,
        "limit",
        "Maximum number of memories to return.",
    );

    schema
}

fn forget_parameters_schema() -> Value {
    let mut schema = schema::object_from_example(
        &json!({
            "memory_id": "mem_0123456789abcdef",
        }),
        &["memory_id"],
    );
    schema::describe(
        &mut schema,
        "memory_id",
        "Memory id to archive so it is no longer recalled by default.",
    );

    schema
}

fn describe_memory_common(schema: &mut Value) {
    schema::describe(
        schema,
        "scope",
        "Memory scope: global for user/profile-level facts, project for current project context.",
    );
    schema::enum_strings(schema, "scope", &["global", "project"]);
    schema::describe(
        schema,
        "kind",
        "Memory type: user preference, feedback, project context, or external reference pointer.",
    );
    schema::enum_strings(
        schema,
        "kind",
        &["user", "feedback", "project", "reference"],
    );
}

async fn remember(memory: &memory::Manager, args: MemoryRememberParams) -> Result<ToolOutcome> {
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
    let outcome = memory
        .remember(draft, args.memory_id.as_deref())
        .await
        .map_err(|err| exec_error("memory_remember", &err))?;
    Ok(json!({
        "success": true,
        "updated_existing": outcome.updated_existing,
        "memory": memory_detail(&outcome.memory),
        "message": if outcome.updated_existing { "Memory updated" } else { "Memory saved" },
    })
    .into())
}

async fn search(memory: &memory::Manager, args: MemorySearchParams) -> Result<ToolOutcome> {
    let matches = memory
        .search(MemoryQuery {
            query: args.query,
            scope: args.scope.map(Into::into),
            kind: args.kind.map(Into::into),
            include_archived: args.include_archived.unwrap_or(false),
            limit: args.limit,
        })
        .await
        .map_err(|err| exec_error("memory_search", &err))?;
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

async fn forget(memory: &memory::Manager, memory_id: String) -> Result<ToolOutcome> {
    let memory = memory
        .forget(&memory_id)
        .await
        .map_err(|err| exec_error("memory_forget", &err))?;
    Ok(json!({
        "success": memory.is_some(),
        "memory_id": memory_id,
        "memory": memory.as_ref().map(memory_summary),
        "message": if memory.is_some() { "Memory archived" } else { "Memory not found" },
    })
    .into())
}

async fn profile(memory: &memory::Manager) -> Result<ToolOutcome> {
    let profile = memory
        .profile()
        .await
        .map_err(|err| exec_error("memory_profile", &err))?;
    Ok(json!({
        "profile": profile_json(&profile),
    })
    .into())
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
                .map_err(|err| {
                    exec_error("memory_remember", format_args!("invalid {field}: {err}"))
                })
        })
        .transpose()
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
    async fn memory_tools_remember_search_profile_and_forget() {
        let dir = tempdir();
        let manager = Arc::new(memory::Manager::new(&dir, &dir.join("project")));
        let remember_tool = MemoryRemember {
            memory: manager.clone(),
        };
        let search_tool = MemorySearch {
            memory: manager.clone(),
        };
        let forget_tool = MemoryForget {
            memory: manager.clone(),
        };
        let profile_tool = MemoryProfile {
            memory: manager.clone(),
        };

        let result = remember_tool
            .call(json!({
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

        let result = search_tool
            .call(json!({
                "query": "concise"
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("memory search should return plain result");
        };
        assert_eq!(value["count"], 1);

        let result = profile_tool.call(json!({})).await.unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("memory profile should return plain result");
        };
        assert!(value["profile"].is_object());

        forget_tool
            .call(json!({
                "memory_id": id
            }))
            .await
            .unwrap();
        let result = search_tool.call(json!({})).await.unwrap();
        let ToolOutcome::Result(value) = result else {
            panic!("memory search should return plain result");
        };
        assert_eq!(value["count"], 0);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[test]
    fn memory_tool_schemas_are_top_level_objects() {
        let dir = tempdir();
        let manager = Arc::new(memory::Manager::new(&dir, &dir.join("project")));
        let remember_schema = MemoryRemember {
            memory: manager.clone(),
        }
        .schema()
        .parameters;
        let search_schema = MemorySearch {
            memory: manager.clone(),
        }
        .schema()
        .parameters;
        let forget_schema = MemoryForget {
            memory: manager.clone(),
        }
        .schema()
        .parameters;
        let profile_schema = MemoryProfile {
            memory: manager.clone(),
        };
        let profile_schema = profile_schema.schema().parameters;

        assert_eq!(remember_schema["type"], "object");
        assert_eq!(
            remember_schema["required"],
            json!(["scope", "kind", "title", "summary", "body"])
        );
        assert_eq!(remember_schema["additionalProperties"], false);
        assert!(remember_schema["properties"].get("action").is_none());
        assert_eq!(
            remember_schema["properties"]["scope"]["enum"],
            json!(["global", "project"])
        );
        assert_eq!(remember_schema["properties"]["memory_id"]["type"], "string");
        assert!(remember_schema["properties"]["title"]["description"].is_string());

        assert_eq!(search_schema["type"], "object");
        assert_eq!(search_schema["required"], json!([]));
        assert_eq!(search_schema["additionalProperties"], false);
        assert!(search_schema["properties"].get("action").is_none());
        assert_eq!(search_schema["properties"]["query"]["type"], "string");

        assert_eq!(forget_schema["type"], "object");
        assert_eq!(forget_schema["required"], json!(["memory_id"]));
        assert_eq!(forget_schema["additionalProperties"], false);

        assert_eq!(profile_schema["type"], "object");
        assert_eq!(profile_schema["properties"], json!({}));
        assert_eq!(profile_schema["additionalProperties"], false);

        for schema in [
            &remember_schema,
            &search_schema,
            &forget_schema,
            &profile_schema,
        ] {
            assert!(!schema_keyword_exists(schema, "oneOf"));
            assert!(!schema_keyword_exists(schema, "anyOf"));
            assert!(!schema_keyword_exists(schema, "allOf"));
            assert!(!schema_type_contains_null(schema));
        }

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
