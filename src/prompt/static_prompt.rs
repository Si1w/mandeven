//! Manifest for built-in static system-prompt sections.
//!
//! Prompt prose lives in Markdown files under `src/prompt/static/` so
//! it can be edited and reviewed as prompt text. This module owns the
//! stable section names and explicit assembly order. The engine
//! remains responsible for combining these static sections with
//! dynamic context such as skills, `AGENTS.md`, and environment info.

/// Section name for the agent identity / first-principles framing.
pub const INTRO_NAME: &str = "intro";

/// Section name for the universal interaction rules.
pub const SYSTEM_RULES_NAME: &str = "system_rules";

/// Section name for the task philosophy / YAGNI guidance.
pub const DOING_TASKS_NAME: &str = "doing_tasks";

/// Section name for the action-safety / blast-radius guidance.
pub const ACTIONS_NAME: &str = "actions";

/// Section name for the tool-selection guidance.
pub const USING_TOOLS_NAME: &str = "using_tools";

/// Section name for the response-style expectations.
pub const TONE_NAME: &str = "tone";

/// One built-in static system-prompt section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticPromptSection {
    /// Stable identifier used for section ordering, cache lookup, and
    /// future `/context`-style introspection.
    pub name: &'static str,
    /// Markdown body embedded in the binary at compile time.
    pub content: &'static str,
}

/// Built-in static system-prompt sections in assembly order.
pub const STATIC_SYSTEM_SECTIONS: &[StaticPromptSection] = &[
    StaticPromptSection {
        name: INTRO_NAME,
        content: include_str!("static/introduction.md"),
    },
    StaticPromptSection {
        name: SYSTEM_RULES_NAME,
        content: include_str!("static/system-rules.md"),
    },
    StaticPromptSection {
        name: DOING_TASKS_NAME,
        content: include_str!("static/doing-tasks.md"),
    },
    StaticPromptSection {
        name: ACTIONS_NAME,
        content: include_str!("static/actions.md"),
    },
    StaticPromptSection {
        name: USING_TOOLS_NAME,
        content: include_str!("static/using-tools.md"),
    },
    StaticPromptSection {
        name: TONE_NAME,
        content: include_str!("static/tone.md"),
    },
];

/// Normalize a static prompt body before it is cached as a section.
#[must_use]
pub fn trim_static(content: &str) -> String {
    content.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(name: &str) -> String {
        STATIC_SYSTEM_SECTIONS
            .iter()
            .find(|section| section.name == name)
            .map(|section| trim_static(section.content))
            .expect("known static section")
    }

    #[test]
    fn static_sections_have_expected_order() {
        let names: Vec<&str> = STATIC_SYSTEM_SECTIONS
            .iter()
            .map(|section| section.name)
            .collect();
        assert_eq!(
            names,
            vec![
                INTRO_NAME,
                SYSTEM_RULES_NAME,
                DOING_TASKS_NAME,
                ACTIONS_NAME,
                USING_TOOLS_NAME,
                TONE_NAME,
            ]
        );
    }

    #[test]
    fn static_sections_trim_trailing_newlines() {
        for section in STATIC_SYSTEM_SECTIONS {
            assert!(
                !trim_static(section.content).ends_with('\n'),
                "template ends with newline after trim: {}",
                section.name
            );
        }
    }

    #[test]
    fn introduction_mentions_first_principles() {
        assert!(content(INTRO_NAME).contains("first principles"));
    }

    #[test]
    fn headed_sections_start_with_their_heading() {
        assert!(content(SYSTEM_RULES_NAME).starts_with("# System\n"));
        assert!(content(DOING_TASKS_NAME).starts_with("# Doing tasks\n"));
        assert!(content(ACTIONS_NAME).starts_with("# Executing actions with care\n"));
        assert!(content(USING_TOOLS_NAME).starts_with("# Using your tools\n"));
        assert!(content(TONE_NAME).starts_with("# Tone and Style\n"));
    }

    #[test]
    fn using_tools_references_real_tool_names() {
        let using_tools = content(USING_TOOLS_NAME);
        for name in [
            "file_read",
            "file_write",
            "file_edit",
            "grep",
            "shell_exec",
            "web_search",
            "web_fetch",
            "task_create",
            "task_update",
            "task_list",
            "task_get",
            "task_run",
            "timer_create",
            "timer_update",
            "timer_list",
            "timer_delete",
            "timer_fire_now",
        ] {
            assert!(
                using_tools.contains(name),
                "using_tools missing reference to `{name}`"
            );
        }
    }

    #[test]
    fn static_prompts_include_execution_and_memory_discipline() {
        assert!(content(DOING_TASKS_NAME).contains("same turn"));
        assert!(content(SYSTEM_RULES_NAME).contains("background context"));
        assert!(content(USING_TOOLS_NAME).contains("Dream background reviewer"));
    }
}
