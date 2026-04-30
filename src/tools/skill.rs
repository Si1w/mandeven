//! `skill_use` — model-facing entry point for invoking a SKILL.md
//! workflow.
//!
//! The tool's call shape is intentionally narrow: a single `skill`
//! parameter naming the workflow to run. The resolved
//! [`crate::skill::Skill`]'s body is spliced into the conversation as
//! a fresh `Message::User` via [`super::ToolOutcome::Inject`], so the
//! model sees the workflow text in user role on the very next turn —
//! the same shape it would see if the user had typed `/<name>`.
//!
//! Mirrors Claude Code's
//! [`SkillTool`](agent-examples/claude-code-analysis/src/tools/SkillTool/SkillTool.ts)
//! "inline" execution mode. The "fork" mode (running the skill as a
//! subagent) is not implemented and is unlikely to be — mandeven has
//! no subagent system today.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use crate::llm::{Message, Tool};
use crate::skill::SkillIndex;

use super::error::{Error, Result};
use super::{BaseTool, ToolOutcome};

/// Tool name used in the registry and on the wire.
pub const SKILL_TOOL_NAME: &str = "skill_use";

#[derive(Deserialize, JsonSchema)]
struct SkillParams {
    /// Name of the skill to invoke. Must match a `name:` in the
    /// `skills_index` system-prompt section.
    skill: String,
}

/// Tool the model calls to invoke a SKILL.md workflow by name.
///
/// Holds an `Arc<SkillIndex>` so it can resolve names against the
/// boot-time catalog. The same `SkillIndex` is shared with the
/// prompt engine (for the `skills_index` section) and the CLI
/// fallback (for `/<name>` user invocation), so all three views see
/// the same set of skills.
pub struct SkillTool {
    index: Arc<SkillIndex>,
}

impl SkillTool {
    /// Construct a tool bound to `index`. The index is loaded once
    /// at boot and never mutated for the run, so the `Arc` is purely
    /// for sharing.
    #[must_use]
    pub fn new(index: Arc<SkillIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl BaseTool for SkillTool {
    fn schema(&self) -> Tool {
        Tool {
            name: SKILL_TOOL_NAME.into(),
            description: "Invoke a skill workflow by name. The skill's instructions \
                are injected into the conversation as a new user message — react to \
                them on the next turn. Available skill names appear in the \
                `skills_index` section of your system prompt; do not guess names \
                that aren't listed there."
                .into(),
            parameters: serde_json::to_value(schema_for!(SkillParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let p: SkillParams =
            serde_json::from_value(args).map_err(|source| Error::InvalidArguments {
                tool: SKILL_TOOL_NAME.into(),
                source,
            })?;

        let skill = self.index.get(&p.skill).ok_or_else(|| Error::Execution {
            tool: SKILL_TOOL_NAME.into(),
            message: format!("unknown skill: {}", p.skill),
        })?;

        // Tool result: a brief acknowledgment so the model knows
        // the call succeeded and can read the user message that
        // follows. Body itself goes in the injected user message.
        let result = Value::String(format!("Loaded skill /{}", skill.frontmatter.name));
        let injected = Message::User {
            content: skill.body.clone(),
        };

        Ok(ToolOutcome::Inject {
            result,
            messages: vec![injected],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, SkillFrontmatter, SkillIndex};
    use std::path::PathBuf;

    fn idx_with(name: &str, body: &str) -> Arc<SkillIndex> {
        Arc::new(SkillIndex::from_skills(vec![Skill {
            frontmatter: SkillFrontmatter {
                name: name.into(),
                description: format!("desc for {name}"),
            },
            body: body.into(),
            source_path: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
        }]))
    }

    #[tokio::test]
    async fn call_returns_inject_with_skill_body_as_user_message() {
        let tool = SkillTool::new(idx_with("git-clean", "## Workflow\nstep 1\nstep 2"));
        let outcome = tool
            .call(serde_json::json!({"skill": "git-clean"}))
            .await
            .unwrap();

        let ToolOutcome::Inject { result, messages } = outcome else {
            panic!("expected Inject");
        };

        // Tool result acknowledges the load.
        let Value::String(s) = result else {
            panic!("expected string result");
        };
        assert!(s.contains("/git-clean"));

        // Injected message carries the body verbatim, in user role.
        assert_eq!(messages.len(), 1);
        let Message::User { content } = &messages[0] else {
            panic!("expected user message");
        };
        assert_eq!(content, "## Workflow\nstep 1\nstep 2");
    }

    #[tokio::test]
    async fn call_unknown_skill_returns_execution_error() {
        let tool = SkillTool::new(idx_with("git-clean", "body"));
        let err = tool
            .call(serde_json::json!({"skill": "nonexistent"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Execution { .. }));
    }

    #[tokio::test]
    async fn call_missing_skill_param_returns_invalid_arguments() {
        let tool = SkillTool::new(idx_with("git-clean", "body"));
        let err = tool.call(serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, Error::InvalidArguments { .. }));
    }
}
