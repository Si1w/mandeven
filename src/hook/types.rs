//! Hook configuration types — what `hooks.json` deserializes into and
//! the [`HookEvent`] enum that names every triggerable lifecycle point.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Lifecycle events that can fire hooks.
///
/// 7 events for v1 — pared down from Claude Code's 28 to the subset
/// mandeven actually produces today (no permission system, no
/// subagents, no IDE integration). Adding more events as features
/// land is a matter of inserting a variant here and a `fire` call at
/// the new trigger point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum HookEvent {
    /// Fires when the agent receives a user message but before the
    /// LLM iteration begins. Payload includes the user prompt text.
    UserPromptSubmit,
    /// Fires before each tool invocation. Payload includes
    /// `tool_name` and `tool_input`. `target = tool_name` for
    /// matcher purposes.
    PreToolUse,
    /// Fires after each tool invocation completes (regardless of
    /// success). Payload adds `tool_response`.
    PostToolUse,
    /// Fires once per session, just after the session metadata is
    /// written but before the first iteration runs.
    SessionStart,
    /// Fires when an iteration completes (one full user-assistant
    /// turn including tool exchanges).
    Stop,
    /// Fires before the compact pipeline rewrites conversation
    /// history. Payload includes `trigger` (`auto` / `manual`).
    PreCompact,
    /// Fires after a successful compact pass. Same payload shape as
    /// `PreCompact`.
    PostCompact,
}

/// One shell-command hook definition.
///
/// Hook `command` is run via `sh -c <command>` (POSIX) or `cmd /c`
/// (Windows — not implemented yet, would land here when needed). The
/// hook receives the event payload as JSON on stdin and a small set
/// of `MANDEVEN_*` env vars; see [`crate::hook::engine`] for details.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommandHook {
    /// Shell command to execute. Whitespace-trimmed at parse time.
    pub command: String,
    /// Per-hook timeout. Falls back to
    /// [`crate::hook::HOOK_TIMEOUT_SECS_DEFAULT`] when absent.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// When `true`, a non-zero exit code from the hook blocks the
    /// surrounding event (e.g. cancels the tool call for
    /// `PreToolUse`, drops the user message for `UserPromptSubmit`).
    /// When `false` (default), non-zero exit is logged but
    /// non-fatal.
    #[serde(default)]
    pub block_on_nonzero_exit: bool,
}

/// One matcher block — a (matcher, hooks) pair that targets a subset
/// of an event's invocations.
///
/// `matcher` is a regex tested against the event's "target" field:
///
/// - `PreToolUse` / `PostToolUse` → tool name
/// - other events → no target; matcher is ignored (treat as
///   "always match")
///
/// Missing matcher = "match everything for this event".
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookMatcher {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<CommandHook>,
}

/// Full hooks.json contents — a flat map keyed by event name.
///
/// Empty events are tolerated; the engine simply has nothing to fire
/// for that event. Unknown event names cause a deserialize error so
/// typos surface at boot rather than at runtime.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HookFile {
    #[serde(flatten)]
    pub events: HashMap<HookEvent, Vec<HookMatcher>>,
}

impl HookFile {
    /// Lookup matcher blocks for `event`, returning an empty slice
    /// when none are configured.
    #[must_use]
    pub fn matchers(&self, event: HookEvent) -> &[HookMatcher] {
        self.events
            .get(&event)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// `true` when no events have any matchers configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.values().all(Vec::is_empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_empty_file_yields_default() {
        let file: HookFile = serde_json::from_str("{}").unwrap();
        assert!(file.is_empty());
    }

    #[test]
    fn deserialize_pretooluse_with_matcher() {
        let raw = r#"{
            "PreToolUse": [
                {
                    "matcher": "shell|file_write",
                    "hooks": [
                        { "command": "deny.sh", "timeout_secs": 5, "block_on_nonzero_exit": true }
                    ]
                }
            ]
        }"#;
        let file: HookFile = serde_json::from_str(raw).unwrap();
        let matchers = file.matchers(HookEvent::PreToolUse);
        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].matcher.as_deref(), Some("shell|file_write"));
        assert_eq!(matchers[0].hooks.len(), 1);
        assert_eq!(matchers[0].hooks[0].command, "deny.sh");
        assert_eq!(matchers[0].hooks[0].timeout_secs, Some(5));
        assert!(matchers[0].hooks[0].block_on_nonzero_exit);
    }

    #[test]
    fn deserialize_rejects_unknown_event_name() {
        let raw = r#"{ "Bogus": [{ "hooks": [{ "command": "x" }] }] }"#;
        let result: serde_json::Result<HookFile> = serde_json::from_str(raw);
        assert!(result.is_err());
    }

    #[test]
    fn matchers_returns_empty_slice_for_unconfigured_event() {
        let file = HookFile::default();
        assert!(file.matchers(HookEvent::Stop).is_empty());
    }
}
