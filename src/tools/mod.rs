//! Tools — capability layer registered with and dispatched by the
//! [`agent`](crate::agent) module.
//!
//! Tools implement [`BaseTool`] and are installed into a [`Registry`].
//! The registry advertises their schemas via [`Registry::schemas`]
//! (placed on [`crate::llm::Request::tools`]) and turns the model's
//! [`ToolCall`]s into [`Message::Tool`] replies via
//! [`Registry::dispatch`]. Dispatch is infallible: any error the tool
//! produces is folded into the reply content so the model can see it
//! on the next turn.
//!
//! Built-in tools live in sibling files ([`mod@file`], [`shell`]) and are
//! registered in bulk via [`register_builtins`]. Stateful primitives
//! such as [`task`] and [`timer`] register through their own modules.

pub(crate) mod dream;
pub mod error;
pub mod file;
pub mod grep;
#[allow(dead_code)]
pub(crate) mod memory;
#[allow(dead_code)]
pub(crate) mod schema;
pub mod shell;
pub mod skill;
pub mod task;
pub mod timer;
pub mod web;

pub use error::{Error, Result};

/// Hard cap on the byte length of any single tool result sent back to
/// the model. All built-in tools (see [`register_builtins`]) truncate
/// at this bound so per-turn context cost stays predictable.
pub const MAX_TOOL_RESULT_BYTES: usize = 30_000;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::llm::{Message, Tool, ToolCall};

/// Outcome of one [`BaseTool::call`].
///
/// 99% of tools return [`Self::Result`] — a JSON value that becomes
/// the [`Message::Tool`] content. The outlier is
/// [`crate::tools::skill::SkillTool`], which uses [`Self::Inject`]
/// to splice an additional [`Message::User`] (the SKILL.md body)
/// into the conversation alongside the regular tool result. This
/// matches Claude Code's `newMessages` mechanism on `SkillTool` (see
/// `agent-examples/claude-code-analysis/src/tools/SkillTool/SkillTool.ts:766`).
///
/// `Result(value)` is freely constructed via `.into()` from any
/// [`serde_json::Value`].
#[derive(Debug)]
pub enum ToolOutcome {
    /// Plain tool result. Becomes a single [`Message::Tool`] in the
    /// conversation.
    Result(serde_json::Value),
    /// Tool result plus extra messages to splice in immediately
    /// after. The agent loop appends each in order, so a `SkillTool`
    /// invocation produces:
    ///
    /// ```text
    /// [tool] result
    /// [user] SKILL.md body
    /// ```
    ///
    /// The next iteration sees the skill body in user role and
    /// reacts.
    Inject {
        /// Payload for the standard [`Message::Tool`] reply.
        result: serde_json::Value,
        /// Extra messages to splice after the tool reply.
        messages: Vec<Message>,
    },
}

impl From<serde_json::Value> for ToolOutcome {
    fn from(value: serde_json::Value) -> Self {
        Self::Result(value)
    }
}

/// Trait implemented by tools that the model can invoke.
///
/// Implementations typically derive their parameter schema from a
/// `#[derive(serde::Deserialize, schemars::JsonSchema)]` struct and
/// build [`Tool::parameters`] via `schemars::schema_for!`.
#[async_trait]
pub trait BaseTool: Send + Sync {
    /// Schema advertised to the model. The `name` field is also used
    /// as the registry key.
    fn schema(&self) -> Tool;

    /// Execute the tool with the parsed arguments object and return
    /// either a plain result or a [`ToolOutcome::Inject`] effect.
    /// Plain JSON values implement `Into<ToolOutcome>` so most tools
    /// can keep returning `Ok(json!({...}))`-style values directly.
    ///
    /// # Errors
    ///
    /// Return [`Error::Execution`] for tool-specific runtime failures.
    async fn call(&self, args: serde_json::Value) -> Result<ToolOutcome>;
}

#[allow(dead_code)]
pub(crate) fn parse_params<T: for<'de> Deserialize<'de>>(
    tool: &'static str,
    args: serde_json::Value,
) -> Result<T> {
    serde_json::from_value(args).map_err(|source| Error::InvalidArguments {
        tool: tool.to_string(),
        source,
    })
}

#[allow(dead_code)]
pub(crate) fn exec_error(tool: &'static str, message: impl std::fmt::Display) -> Error {
    Error::Execution {
        tool: tool.to_string(),
        message: message.to_string(),
    }
}

/// Registry of tools available to the agent.
///
/// Backed by [`BTreeMap`] so [`Self::schemas`] emits tools in a stable
/// lexicographic order that survives process restarts. The iteration
/// order of [`std::collections::HashMap`] is deterministic within one
/// run but randomized across runs, which silently invalidates
/// `DeepSeek`'s automatic prefix cache between sessions — same prompt
/// bytes, different tool ordering on the wire, fresh cache miss.
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn BaseTool>>,
}

impl Registry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    /// Register a tool under the name from its [`Tool::name`].
    ///
    /// A later registration under the same name replaces the earlier
    /// one.
    pub fn register(&mut self, tool: Arc<dyn BaseTool>) {
        let name = tool.schema().name;
        self.tools.insert(name, tool);
    }

    /// Emit the wire schemas for every registered tool, for inclusion
    /// in an LLM [`crate::llm::Request::tools`]. An empty registry
    /// yields an empty `Vec`, which signals "no tools advertised" to
    /// the provider layer.
    #[must_use]
    pub fn schemas(&self) -> Vec<Tool> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    /// Dispatch a batch of tool invocations from the model.
    ///
    /// Always produces at least one [`Message::Tool`] per input
    /// call (success → JSON result; failure → `{"error":"..."}`
    /// object). Tools that return [`ToolOutcome::Inject`] additionally
    /// splice the extra messages **after** their own tool reply, so
    /// the output `Vec` may be longer than `calls.len()`.
    ///
    /// Used by callers that don't need per-call hook orchestration.
    /// The agent loop instead drives one call at a time through
    /// [`Self::invoke_to_messages`] so it can fire `PreToolUse` /
    /// `PostToolUse` hooks around each invocation.
    pub async fn dispatch(&self, calls: Vec<ToolCall>) -> Vec<Message> {
        let mut out = Vec::with_capacity(calls.len());
        for call in calls {
            out.extend(self.invoke_to_messages(call).await);
        }
        out
    }

    /// Run one tool call and translate the outcome into the agent's
    /// [`Message`] sequence. Mirrors the per-call branch of
    /// [`Self::dispatch`] but exposed as a public single-call entry
    /// point so the agent layer can interleave hook firings.
    pub async fn invoke_to_messages(&self, call: ToolCall) -> Vec<Message> {
        match self.invoke(&call).await {
            Ok(ToolOutcome::Result(value)) => vec![Message::Tool {
                content: serialize_result(&value),
                tool_call_id: call.id,
            }],
            Ok(ToolOutcome::Inject { result, messages }) => {
                let mut out = Vec::with_capacity(messages.len() + 1);
                out.push(Message::Tool {
                    content: serialize_result(&result),
                    tool_call_id: call.id,
                });
                out.extend(messages);
                out
            }
            Err(err) => vec![Message::Tool {
                content: error_content(&err.to_string()),
                tool_call_id: call.id,
            }],
        }
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutcome> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| Error::UnknownTool(call.name.clone()))?;
        let args = parse_arguments(&call.name, &call.arguments)?;
        tool.call(args).await
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a model-emitted `arguments` string into a JSON value, tagging
/// any parse error with the tool name.
fn parse_arguments(tool: &str, raw: &str) -> Result<serde_json::Value> {
    serde_json::from_str(raw).map_err(|source| Error::InvalidArguments {
        tool: tool.to_string(),
        source,
    })
}

/// Serialize a tool's successful return value.
///
/// Plain string returns are passed through unquoted so the model sees
/// raw text (e.g. `Wrote 42 bytes to ...`) rather than a JSON-escaped
/// string (`"Wrote 42 bytes to ..."`). Other [`serde_json::Value`]
/// shapes are JSON-serialized; the operation cannot fail in practice
/// because `Value` is always valid JSON by construction.
fn serialize_result(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).expect("serde_json::Value always serializes"),
    }
}

/// Wrap an error message as a JSON object the model can parse on the
/// next turn.
fn error_content(message: &str) -> String {
    serde_json::to_string(&json!({ "error": message }))
        .expect("fixed-shape error object always serializes")
}

/// Install the always-on, stateless built-in tool set ([`file::FileRead`],
/// [`file::FileWrite`], [`file::FileEdit`], [`grep::Grep`],
/// [`shell::Shell`], [`web::WebSearch`], [`web::WebFetch`]) into
/// `registry`.
///
/// Stateful or config-gated tools register through their own modules because
/// they need runtime handles: [`task::register`], [`timer::register`], and
/// [`skill::SkillTool`].
/// Callers who want a different subset can register tools directly instead.
pub fn register_builtins(registry: &mut Registry) {
    registry.register(Arc::new(file::FileRead));
    registry.register(Arc::new(file::FileWrite));
    registry.register(Arc::new(file::FileEdit));
    registry.register(Arc::new(grep::Grep));
    registry.register(Arc::new(shell::Shell));
    registry.register(Arc::new(web::WebSearch));
    registry.register(Arc::new(web::WebFetch));
}
