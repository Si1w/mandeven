//! Single-purpose prompts that bypass the
//! [`crate::prompt::SystemPrompt`] assembly pipeline.
//!
//! Title generation, the heartbeat phase-1 decide call, and the
//! compact-summary call each pin the model to one structured task.
//! Folding them into the main iteration system prompt would only
//! confuse a long-running session — they're physically separate so
//! the agent can wire them in at exactly one call site each.
//!
//! Mirrors Claude Code's `services/compact/prompt.ts` and
//! `services/SessionMemory/prompts.ts` pattern (see
//! [`agent-examples/claude-code-analysis/analysis/04g-prompt-management.md`]
//! §9): a per-task protocol prompt that happens not to share *any*
//! sections with the main system prompt.

use chrono::{DateTime, Utc};

use crate::llm::Message;

/// System prompt that asks the model for a short, descriptive
/// session title from the user's first message. Migrated verbatim
/// from the previous inline constant on
/// [`crate::agent::Agent::generate_title`].
pub const TITLE_SYSTEM: &str = "Generate a short, descriptive title (max 8 words) for a conversation \
     starting with the following user message. Reply with only the title, \
     no quotes or punctuation.";

/// System prompt for heartbeat phase-1 — constrains the model to a
/// single `heartbeat_decide` tool call so the answer is structured
/// rather than free text. Migrated verbatim from
/// [`crate::agent::Agent::heartbeat_decide`].
pub const HEARTBEAT_DECIDE_SYSTEM: &str = "You are the heartbeat decision step. \
    Read the heartbeat checklist provided and call the heartbeat_decide tool exactly once. \
    Use action=\"skip\" when nothing in the checklist needs attention right now. \
    Use action=\"run\" with a concise one-or-two-sentence summary in `tasks` when at \
    least one item should be acted on now.";

/// Base instructions for the conversation-compaction summarizer.
/// Tuned for "preserve what later turns will need" rather than "make
/// it short". Migrated verbatim from
/// [`crate::agent::compact::COMPACT_SYSTEM_PROMPT`].
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

/// Build the heartbeat phase-1 message envelope. The user message
/// includes the current time and the resolved `HEARTBEAT.md` body so
/// the model has both the schedule context and the task list.
#[must_use]
pub fn heartbeat_decide_messages(content: &str, now: DateTime<Utc>) -> Vec<Message> {
    vec![
        Message::System {
            content: HEARTBEAT_DECIDE_SYSTEM.into(),
        },
        Message::User {
            content: format!("Current time: {now}\n\nHEARTBEAT.md contents:\n\n{content}"),
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
    fn heartbeat_decide_messages_embed_time_and_content() {
        let now = DateTime::parse_from_rfc3339("2026-04-26T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let msgs = heartbeat_decide_messages("- check inbox", now);
        let Message::User { content } = &msgs[1] else {
            panic!("expected user");
        };
        assert!(content.contains("2026-04-26 08:00:00"));
        assert!(content.contains("- check inbox"));
    }

    #[test]
    fn compact_summary_system_appends_focus() {
        let base = compact_summary_system(None);
        let focused = compact_summary_system(Some("recent file edits"));
        assert!(focused.starts_with(&base));
        assert!(focused.contains("recent file edits"));
    }
}
