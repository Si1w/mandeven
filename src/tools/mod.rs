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

pub mod error;

pub use error::{Error, Result};

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::llm::{Message, Tool, ToolCall};

/// Trait implemented by skills that expose a callable tool to the
/// model.
///
/// Implementations typically derive their parameter schema from a
/// `#[derive(serde::Deserialize, schemars::JsonSchema)]` struct and
/// build [`Tool::parameters`] via `schemars::schema_for!`.
#[async_trait]
pub trait BaseTool: Send + Sync {
    /// Schema advertised to the model. The `name` field is also used
    /// as the registry key.
    fn schema(&self) -> Tool;

    /// Execute the tool with the parsed arguments object and return a
    /// structured JSON result. The registry serializes the returned
    /// value into the string payload of [`Message::Tool`].
    ///
    /// # Errors
    ///
    /// Return [`Error::Execution`] for tool-specific runtime failures.
    async fn call(&self, args: serde_json::Value) -> Result<serde_json::Value>;
}

/// Registry of tools available to the agent.
pub struct Registry {
    tools: HashMap<String, Arc<dyn BaseTool>>,
}

impl Registry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
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

    /// Dispatch a batch of tool invocations from the model. Always
    /// produces exactly one [`Message::Tool`] per input call.
    /// Successes carry the tool's JSON result; failures carry an
    /// `{"error":"..."}` object so the model can react on the next
    /// turn.
    pub async fn dispatch(&self, calls: Vec<ToolCall>) -> Vec<Message> {
        let mut out = Vec::with_capacity(calls.len());
        for call in calls {
            let content = match self.invoke(&call).await {
                Ok(value) => serialize_result(&value),
                Err(err) => error_content(&err.to_string()),
            };
            out.push(Message::Tool {
                content,
                tool_call_id: call.id,
            });
        }
        out
    }

    async fn invoke(&self, call: &ToolCall) -> Result<serde_json::Value> {
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

/// Serialize a tool's successful return value. `serde_json::Value` is
/// always valid JSON by construction, so serialization cannot fail in
/// practice.
fn serialize_result(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value always serializes")
}

/// Wrap an error message as a JSON object the model can parse on the
/// next turn.
fn error_content(message: &str) -> String {
    serde_json::to_string(&json!({ "error": message }))
        .expect("fixed-shape error object always serializes")
}
