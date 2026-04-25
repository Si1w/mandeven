//! Schemas for heartbeat-only tools.
//!
//! Sits next to [`super::file`] / [`super::shell`] to share the
//! schema-construction idiom, but **does not** implement
//! [`super::BaseTool`]: the `heartbeat_decide` tool is constructed
//! ad-hoc during heartbeat phase-1 and never installed in the global
//! [`super::Registry`]. Keeping it out of the registry is exactly
//! what hides it from normal user turns — the agent only advertises
//! it on the one phase-1 LLM call where it is required.

use serde::Deserialize;
use serde_json::json;

use crate::llm::Tool;

/// Tool name. Phase-1 enforces an exact match before honoring the
/// model's response.
pub const HEARTBEAT_DECIDE_TOOL_NAME: &str = "heartbeat_decide";

/// Build the tool spec advertised during heartbeat phase-1.
///
/// Constructed fresh on each call rather than memoized — the schema
/// is small and [`Tool`] is plain data. Avoids the
/// `Lazy<Tool>`-style ceremony for almost no win.
#[must_use]
pub fn heartbeat_decide_tool() -> Tool {
    Tool {
        name: HEARTBEAT_DECIDE_TOOL_NAME.to_string(),
        description: "Report whether anything in the heartbeat checklist needs attention now."
            .into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["skip", "run"],
                    "description": "skip = nothing needs attention; run = at least one task should run now"
                },
                "tasks": {
                    "type": "string",
                    "description": "Concise summary of which tasks should run. Required when action=run."
                }
            },
            "required": ["action"]
        }),
    }
}

/// Wire shape of the tool's arguments.
///
/// `Default` lands on `action = ""`, which the caller treats as
/// `Skip`. So malformed JSON folds into "stay silent" rather than
/// surfacing an error to the user.
#[derive(Debug, Default, Deserialize)]
pub struct HeartbeatDecideArgs {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub tasks: String,
}
