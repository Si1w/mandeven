//! Section primitives — the building blocks of an assembled
//! [`SystemPrompt`].
//!
//! Loosely modeled on Claude Code's `systemPromptSection` /
//! `clearSystemPromptSections` design (see
//! `agent-examples/claude-code-analysis/src/constants/systemPromptSections.ts`),
//! pared down to what mandeven needs today:
//!
//! - One named slice of system-prompt text per [`Section`].
//! - A [`SectionCache`] that memoizes section content by name so the
//!   bytes fed to the model are byte-identical turn after turn,
//!   keeping `DeepSeek`'s prefix cache hot.
//!
//! Every section is cacheable; explicit invalidation goes through
//! [`SectionCache::clear`], wired to `/compact` so the cache rebuilds
//! against the new prefix. There is no per-section "bypass cache" flag
//! — Claude Code's `DANGEROUS_uncachedSystemPromptSection` exists to
//! handle MCP servers that connect/disconnect mid-session, and
//! mandeven has no equivalent volatile state. Reintroduce one if a
//! genuinely-mid-run-mutating section appears.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::llm::Message;

/// One named slice of system-prompt text.
///
/// `name` is `&'static str` so cache lookups are pointer-equality
/// fast and so every section's identity is unambiguous in
/// `/context`-style introspection.
#[derive(Clone, Debug)]
pub struct Section {
    /// Stable identifier for cache lookup and `/context` accounting.
    pub name: &'static str,
    /// Rendered text. Joined into the final system message with
    /// `\n\n` separators.
    pub content: String,
}

/// Ordered, named sections that compose into a single system message.
///
/// The `\n\n`-joined rendering happens at [`Self::into_message`]; up
/// until then sections are addressable by name for cache management
/// and per-section token accounting.
#[derive(Debug, Default)]
pub struct SystemPrompt {
    sections: Vec<Section>,
}

impl SystemPrompt {
    /// Construct an empty prompt.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `section`. Sections are emitted in insertion order;
    /// callers control the ordering at the build site.
    pub fn push(&mut self, section: Section) {
        self.sections.push(section);
    }

    /// Borrow the underlying section list — used by `/context`-style
    /// callers that want token counts per name.
    pub fn iter_named(&self) -> impl Iterator<Item = (&str, &str)> {
        self.sections.iter().map(|s| (s.name, s.content.as_str()))
    }

    /// `true` when no sections have been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Render to a single [`Message::System`]. Sections are joined
    /// with `\n\n` so they read as separated paragraphs to the model.
    /// An empty prompt produces an empty `System` message — callers
    /// that want to skip the `System` slot entirely should test
    /// [`Self::is_empty`] first.
    #[must_use]
    pub fn into_message(self) -> Message {
        let content = self
            .sections
            .into_iter()
            .map(|s| s.content)
            .collect::<Vec<_>>()
            .join("\n\n");
        Message::System { content }
    }
}

/// Memoizes [`Section`] content keyed by name.
///
/// Lookup is `O(1)` under a `Mutex<HashMap>` — section count is in
/// the single digits today and is unlikely to ever justify a sharded
/// or lock-free structure. The cache is process-lifetime: entries are
/// only dropped on [`Self::clear`], which the agent invokes from
/// `/compact` (and the future `/clear` / `/reload-prompt` paths) to
/// match Claude Code's
/// [`clearSystemPromptSections`](https://github.com/anthropic/claude-code/blob/main/src/constants/systemPromptSections.ts)
/// timing.
#[derive(Debug, Default)]
pub struct SectionCache {
    inner: Mutex<HashMap<&'static str, String>>,
}

impl SectionCache {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached content for `name`, computing it via
    /// `compute` on a miss.
    ///
    /// # Panics
    ///
    /// Panics if the cache mutex was poisoned by a prior compute
    /// call. Computes are pure formatting + I/O; a panic inside one
    /// is irrecoverable and the surfacing here is the honest answer.
    pub fn get_or_compute<F>(&self, name: &'static str, compute: F) -> String
    where
        F: FnOnce() -> String,
    {
        let mut map = self.inner.lock().expect("section cache poisoned");
        if let Some(v) = map.get(name) {
            return v.clone();
        }
        let v = compute();
        map.insert(name, v.clone());
        v
    }

    /// Drop every cached entry. Called on `/compact`, and (future)
    /// `/clear` / `/reload-prompt`.
    ///
    /// # Panics
    ///
    /// Panics if the cache mutex was poisoned.
    pub fn clear(&self) {
        self.inner.lock().expect("section cache poisoned").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(name: &'static str, content: &str) -> Section {
        Section {
            name,
            content: content.into(),
        }
    }

    #[test]
    fn into_message_joins_sections_with_double_newlines() {
        let mut p = SystemPrompt::new();
        p.push(section("a", "alpha"));
        p.push(section("b", "bravo"));
        let Message::System { content } = p.into_message() else {
            panic!("expected system message");
        };
        assert_eq!(content, "alpha\n\nbravo");
    }

    #[test]
    fn iter_named_yields_insertion_order() {
        let mut p = SystemPrompt::new();
        p.push(section("first", "1"));
        p.push(section("second", "2"));
        p.push(section("third", "3"));
        let names: Vec<_> = p.iter_named().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn empty_prompt_renders_empty_system_message() {
        let p = SystemPrompt::new();
        assert!(p.is_empty());
        let Message::System { content } = p.into_message() else {
            panic!("expected system message");
        };
        assert!(content.is_empty());
    }

    #[test]
    fn cache_returns_stored_value_on_second_lookup() {
        let cache = SectionCache::new();
        let mut calls = 0;
        let _ = cache.get_or_compute("intro", || {
            calls += 1;
            "first".to_string()
        });
        let v = cache.get_or_compute("intro", || {
            calls += 1;
            "second".to_string()
        });
        assert_eq!(v, "first");
        assert_eq!(calls, 1);
    }

    #[test]
    fn clear_drops_cached_entries() {
        let cache = SectionCache::new();
        let _ = cache.get_or_compute("intro", || "before".to_string());
        cache.clear();
        let v = cache.get_or_compute("intro", || "after".to_string());
        assert_eq!(v, "after");
    }
}
