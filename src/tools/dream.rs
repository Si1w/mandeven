//! Internal structured tool used by the Dream background reviewer.
//!
//! This module is intentionally not registered in the foreground tool registry:
//! Dream calls the LLM with this single tool in its own quiet review pass.

use serde::Deserialize;
use serde_json::json;

use crate::llm::Tool;
use crate::memory::{MemoryDraft, MemoryKind, MemoryScope, MemorySource, MemorySourceKind};

/// Tool name required from the Dream extraction call.
pub(crate) const DREAM_EXTRACT_TOOL_NAME: &str = "dream_extract";

/// Build the Dream system prompt for the configured extraction budget.
#[must_use]
pub(crate) fn system_prompt(max_candidates: usize) -> String {
    format!(
        "You are Mandeven Dream, a quiet background memory reviewer. \
         You review session evidence and call dream_extract exactly once. \
         Extract at most {max_candidates} durable global memories that will reduce future user steering. \
         Use global/user for user facts or preferences. Use global/feedback for \
         corrections about how the assistant should communicate or work. \
         Do not create project memories; project-local context belongs in AGENTS.md. \
         Do not save secrets, transient task progress, completed-work diaries, \
         raw logs, or procedures better represented as skills. If nothing is worth keeping, \
         call dream_extract with an empty memories array."
    )
}

/// Tool schema for Dream's structured extraction response.
#[must_use]
pub(crate) fn extract_tool(max_candidates: usize) -> Tool {
    Tool {
        name: DREAM_EXTRACT_TOOL_NAME.to_string(),
        description: "Return durable global memory candidates extracted from session evidence."
            .to_string(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "memories": {
                    "type": "array",
                    "maxItems": max_candidates,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "scope": {
                                "type": "string",
                                "enum": ["global"]
                            },
                            "kind": {
                                "type": "string",
                                "enum": ["user", "feedback"]
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

/// Parsed arguments returned by the Dream extraction tool.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct DreamExtractArgs {
    /// Candidate global memories.
    #[serde(default)]
    pub(crate) memories: Vec<DreamMemoryCandidate>,
}

/// One raw memory candidate produced by Dream extraction.
#[derive(Debug, Deserialize)]
pub(crate) struct DreamMemoryCandidate {
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
    /// Convert a raw candidate into a validated global memory draft.
    pub(crate) fn into_draft(self) -> std::result::Result<MemoryDraft, String> {
        let kind = parse_kind(&self.kind)?;
        let scope = parse_scope(&self.scope)?;
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
        other => Err(format!(
            "invalid dream memory scope {other:?}; only global is supported"
        )),
    }
}

fn parse_kind(raw: &str) -> std::result::Result<MemoryKind, String> {
    match raw {
        "user" => Ok(MemoryKind::User),
        "feedback" => Ok(MemoryKind::Feedback),
        other => Err(format!(
            "invalid dream memory kind {other:?}; only user and feedback are supported"
        )),
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
