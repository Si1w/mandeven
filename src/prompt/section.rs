//! Section primitives — the building blocks of an assembled
//! [`SystemPrompt`].
//!
//! Mirrors Claude Code's `systemPromptSection` /
//! `DANGEROUS_uncachedSystemPromptSection` design (see
//! [`agent-examples/claude-code-analysis/src/constants/systemPromptSections.ts`])
//! pared down to what mandeven needs today:
//!
//! - One named slice of system-prompt text per [`Section`].
//! - A `cache_break` flag marking sections expected to vary across
//!   turns.
//! - A [`SectionCache`] that memoizes stable section content so the
//!   bytes fed to the model are byte-identical turn after turn,
//!   keeping `DeepSeek`'s prefix cache hot.
//!
//! There is no on-the-wire boundary marker (Claude Code's
//! `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__`): the `cache_break` field
//! itself plus the ordering invariant enforced in [`SystemPrompt::push`]
//! carry the same information without polluting the prompt with a
//! sentinel string the model would have to read.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::llm::Message;

/// One named slice of system-prompt text.
///
/// `name` is `&'static str` so cache lookups are pointer-equality
/// fast and so every section's identity is unambiguous in
/// `/context`-style introspection. `cache_break` is informational at
/// the type level and load-bearing at the assembly layer:
/// [`SystemPrompt::push`] forbids placing a stable section after a
/// volatile one, which is what gives downstream prefix caches
/// (`DeepSeek`, future `Anthropic` with `cache_control`) something
/// contiguous to anchor on.
#[derive(Clone, Debug)]
pub struct Section {
    /// Stable identifier for cache lookup and `/context` accounting.
    pub name: &'static str,
    /// Rendered text. Joined into the final system message with
    /// `\n\n` separators.
    pub content: String,
    /// `true` ⇒ content is expected to vary across calls and must be
    /// placed AFTER all stable sections.
    pub cache_break: bool,
}

/// Ordered, named sections that compose into a single system message.
///
/// The `\n\n`-joined rendering happens at [`Self::into_message`]; up
/// until then sections are addressable by name for cache management
/// and per-section token accounting.
#[derive(Debug, Default)]
pub struct SystemPrompt {
    sections: Vec<Section>,
    /// Latches once any `cache_break: true` section has been pushed.
    /// Subsequent pushes of `cache_break: false` violate the
    /// stable-prefix-then-volatile-suffix discipline and are caught
    /// by [`Self::push`]'s debug assertion.
    has_volatile: bool,
}

impl SystemPrompt {
    /// Construct an empty prompt.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `section`. Panics in debug builds when the ordering
    /// invariant ("stable sections precede every volatile section")
    /// is violated — release builds accept the ordering and let the
    /// degraded prefix cache hit be the only consequence.
    ///
    /// The invariant is invariant rather than soft because a single
    /// out-of-order push pushes the cache miss point all the way to
    /// the start of the offending section, throwing away every
    /// downstream `cache_break: true` slot's stability guarantee.
    pub fn push(&mut self, section: Section) {
        debug_assert!(
            !self.has_volatile || section.cache_break,
            "stable section {:?} pushed after a volatile section — \
             violates the prefix-cache discipline",
            section.name,
        );
        if section.cache_break {
            self.has_volatile = true;
        }
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
///
/// `cache_break: true` sections always bypass the cache — that's the
/// whole point of the flag — so mutating-state callers don't need to
/// reach for `clear()` after each volatile recompute.
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
    /// `compute` on a miss. When `cache_break` is `true` the cache
    /// is bypassed entirely — `compute` runs and the result is not
    /// stored.
    ///
    /// # Panics
    ///
    /// Panics if the cache mutex was poisoned by a prior compute
    /// call. Computes are pure formatting + I/O; a panic inside one
    /// is irrecoverable and the surfacing here is the honest answer.
    pub fn get_or_compute<F>(&self, name: &'static str, cache_break: bool, compute: F) -> String
    where
        F: FnOnce() -> String,
    {
        if cache_break {
            return compute();
        }
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

    fn stable(name: &'static str, content: &str) -> Section {
        Section {
            name,
            content: content.into(),
            cache_break: false,
        }
    }

    fn volatile(name: &'static str, content: &str) -> Section {
        Section {
            name,
            content: content.into(),
            cache_break: true,
        }
    }

    #[test]
    fn into_message_joins_sections_with_double_newlines() {
        let mut p = SystemPrompt::new();
        p.push(stable("a", "alpha"));
        p.push(stable("b", "bravo"));
        let Message::System { content } = p.into_message() else {
            panic!("expected system message");
        };
        assert_eq!(content, "alpha\n\nbravo");
    }

    #[test]
    fn iter_named_yields_insertion_order() {
        let mut p = SystemPrompt::new();
        p.push(stable("first", "1"));
        p.push(stable("second", "2"));
        p.push(volatile("third", "3"));
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
    #[should_panic(expected = "violates the prefix-cache discipline")]
    fn push_stable_after_volatile_panics_in_debug() {
        let mut p = SystemPrompt::new();
        p.push(volatile("env", "now=..."));
        // Should panic: would push a stable section after a volatile
        // one, breaking the prefix-cache discipline.
        p.push(stable("intro", "..."));
    }

    #[test]
    fn cache_returns_stored_value_on_second_lookup() {
        let cache = SectionCache::new();
        let mut calls = 0;
        let _ = cache.get_or_compute("intro", false, || {
            calls += 1;
            "first".to_string()
        });
        let v = cache.get_or_compute("intro", false, || {
            calls += 1;
            "second".to_string()
        });
        assert_eq!(v, "first");
        assert_eq!(calls, 1);
    }

    #[test]
    fn cache_break_bypasses_storage() {
        let cache = SectionCache::new();
        let mut calls = 0;
        let _ = cache.get_or_compute("env", true, || {
            calls += 1;
            "first".to_string()
        });
        let v = cache.get_or_compute("env", true, || {
            calls += 1;
            "second".to_string()
        });
        // Both computes ran; second value was returned.
        assert_eq!(v, "second");
        assert_eq!(calls, 2);
    }

    #[test]
    fn clear_drops_cached_entries() {
        let cache = SectionCache::new();
        let _ = cache.get_or_compute("intro", false, || "before".to_string());
        cache.clear();
        let v = cache.get_or_compute("intro", false, || "after".to_string());
        assert_eq!(v, "after");
    }
}
