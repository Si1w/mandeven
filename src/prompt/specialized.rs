//! Single-purpose prompts that bypass the
//! [`crate::prompt::SystemPrompt`] assembly pipeline.
//!
//! Title generation and the compact-summary call each pin the model
//! to one structured task.
//! Folding them into the main iteration system prompt would only
//! confuse a long-running session — they're physically separate so
//! the agent can wire them in at exactly one call site each.
//!
//! Mirrors Claude Code's `services/compact/prompt.ts` and
//! `services/SessionMemory/prompts.ts` pattern (see
//! `agent-examples/claude-code-analysis/analysis/04g-prompt-management.md`
//! §9): a per-task protocol prompt that happens not to share *any*
//! sections with the main system prompt.

use crate::llm::Message;

/// System prompt that asks the model for a short, descriptive
/// session title from the user's first message.
pub const TITLE_SYSTEM: &str = "Generate a short, descriptive title (max 8 words) for a conversation \
     starting with the following user message. Reply with only the title, \
     no quotes or punctuation.";

/// Base instructions for the conversation-compaction summarizer.
/// Tuned for "preserve what later turns will need" rather than "make
/// it short".
pub const COMPACT_SUMMARY_SYSTEM: &str = "You are summarizing the older portion of a conversation \
between a user and an AI agent. Produce a concise prose summary that preserves: \
(1) the user's goals and any standing constraints they declared; \
(2) decisions made and the reasoning behind them; \
(3) tool results that the assistant later relied on; \
(4) anything the assistant promised to do later. \
Drop chitchat and verbose tool output. Output ONLY the summary text, \
no preamble, no closing remarks.";

/// Build the two-message envelope (`System` + `User`) the title
/// generation call sends to the model.
#[must_use]
pub fn title_messages(user_input: &str) -> Vec<Message> {
    vec![
        Message::System {
            content: TITLE_SYSTEM.into(),
        },
        Message::User {
            content: user_input.into(),
        },
    ]
}

/// Render the compact-summary system prompt, optionally extending it
/// with a user-supplied focus area.
///
/// Migrated from `agent::compact::build_summary_system_prompt`. The
/// focus suffix is the **only** reason this is a function and not a
/// constant — every other variation we've added so far would have
/// fit in the static text.
#[must_use]
pub fn compact_summary_system(focus: Option<&str>) -> String {
    match focus {
        None => COMPACT_SUMMARY_SYSTEM.to_string(),
        Some(f) => format!(
            "{COMPACT_SUMMARY_SYSTEM}\n\nUser-supplied focus area (prioritize when summarizing): {f}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_messages_pair_system_then_user() {
        let msgs = title_messages("first message body");
        assert_eq!(msgs.len(), 2);
        match &msgs[0] {
            Message::System { content } => assert_eq!(content, TITLE_SYSTEM),
            _ => panic!("expected system"),
        }
        match &msgs[1] {
            Message::User { content } => assert_eq!(content, "first message body"),
            _ => panic!("expected user"),
        }
    }

    #[test]
    fn compact_summary_system_appends_focus() {
        let base = compact_summary_system(None);
        let focused = compact_summary_system(Some("recent file edits"));
        assert!(focused.starts_with(&base));
        assert!(focused.contains("recent file edits"));
    }
}
