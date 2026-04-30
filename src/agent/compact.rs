//! Conversation compaction.
//!
//! Mirrors Claude Code's auto-compact pipeline as documented in
//! `agent-examples/claude-code-analysis/analysis/04f-context-management.md`,
//! adapted to our smaller surface (single channel, no images, no
//! file/plan/skill state to re-inject):
//!
//! 1. **Effective window** = `max_context_window * (1 - summary_output_reserve_pct/100)`,
//!    leaving the summary call room to write its own answer.
//! 2. **Trigger** when estimated tokens cross
//!    `effective_window * (1 - trigger_buffer_pct/100)`. Char-count / 4
//!    estimation matches Claude Code's `roughTokenCountEstimation`.
//! 3. **Partition** rear-loaded preserve region (token-budget driven,
//!    not "last N rounds"): from the end backwards, accumulate until
//!    `preserve_min_pct` of the window is met *and* at least
//!    `preserve_min_text_messages` text messages are included,
//!    capped at `preserve_max_pct`. Boundary aligned so a
//!    tool-call/tool-result pair is never split.
//! 4. **Summarize** via a single non-streaming LLM call. PTL
//!    fallback peels older messages and retries up to
//!    `max_ptl_retries` times.
//! 5. **Assemble** = system messages + `Message::Compact { … }` +
//!    preserve region. The boundary message degrades to a `system`
//!    role on the wire but is stored as its own variant for UI +
//!    audit purposes.
//! 6. **Circuit breaker** on `max_consecutive_failures` consecutive
//!    summarize failures stops the loop from burning tokens against
//!    a permanently-too-large session.
//!
//! Why percentages instead of fixed token counts: providers we
//! support range from 256K (Mistral) to 1M (`DeepSeek` v4-flash). A
//! `13_000` token buffer is 6.5% on the former and 1.3% on the
//! latter — same setting, very different aggressiveness. Keeping
//! the knobs in `u8` percent space gives one config that scales
//! with the model.
//!
//! Prefix-cache implications: every compaction rewrites the prefix,
//! so `DeepSeek`'s server-side prompt cache resets afterwards. That is
//! the reason for a generous trigger buffer — frequent compaction
//! defeats the cache.

use serde::{Deserialize, Serialize};

use crate::config::LLMProfile;
use crate::llm::{BaseLLMClient, CompactBoundary, CompactTrigger, Message, Request, Thinking};

/// Char-count divisor used by [`estimate_tokens`]. Matches Claude
/// Code's `roughTokenCountEstimation` heuristic — under-estimates
/// for CJK, over-estimates for code-heavy content, but is zero-dep
/// and good enough for trigger logic. Replace with a real tokenizer
/// when accuracy matters.
const CHARS_PER_TOKEN: usize = 4;

/// Defaults chosen to match Claude Code's published 200K-tuned values
/// (`AUTOCOMPACT_BUFFER_TOKENS = 13_000`,
/// `MAX_OUTPUT_TOKENS_FOR_SUMMARY = 20_000`,
/// `DEFAULT_SM_COMPACT_CONFIG.{minTokens=10_000, maxTokens=40_000}`)
/// expressed as percentages of the model's `max_context_window`.
const DEFAULT_TRIGGER_BUFFER_PCT: u8 = 7;
const DEFAULT_SUMMARY_OUTPUT_RESERVE_PCT: u8 = 10;
const DEFAULT_PRESERVE_MIN_PCT: u8 = 5;
const DEFAULT_PRESERVE_MAX_PCT: u8 = 20;
const DEFAULT_PRESERVE_MIN_TEXT_MESSAGES: usize = 5;
const DEFAULT_MAX_CONSECUTIVE_FAILURES: u8 = 3;
const DEFAULT_MAX_PTL_RETRIES: u8 = 2;

/// Configuration for the compact pipeline. Defaults mirror Claude
/// Code's published values rebased into model-relative percentages;
/// tune via `[agent.compact]` in `mandeven.toml`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactConfig {
    /// Trigger when estimated tokens are within this percent of the
    /// effective context window. Larger ⇒ compact earlier (more
    /// headroom, more cache breaks); smaller ⇒ compact later (closer
    /// to the limit, more risk of mid-call failure).
    #[serde(default = "default_trigger_buffer_pct")]
    pub trigger_buffer_pct: u8,

    /// Percent of `LLMProfile::max_context_window` reserved for the
    /// summary call's own output. Without this carve-out the
    /// summarize request itself can blow past the limit.
    #[serde(default = "default_summary_output_reserve_pct")]
    pub summary_output_reserve_pct: u8,

    /// Lower bound on preserve-region size, expressed as a percent
    /// of `max_context_window`. Stop growing the preserve region
    /// once this is met (and `preserve_min_text_messages` is met).
    #[serde(default = "default_preserve_min_pct")]
    pub preserve_min_pct: u8,

    /// Upper bound on preserve-region size, percent of
    /// `max_context_window`. Hard cap so a single huge tool result
    /// can't strand the entire preserve budget.
    #[serde(default = "default_preserve_max_pct")]
    pub preserve_max_pct: u8,

    /// Lower bound on `User` / `Assistant` text messages kept,
    /// independent of token count. Prevents pathological cases
    /// (one giant tool result eating the whole budget) from
    /// stranding the preserve region with no actual conversation.
    #[serde(default = "default_preserve_min_text_messages")]
    pub preserve_min_text_messages: usize,

    /// Number of consecutive `compact_messages` failures before the
    /// circuit breaker opens and the agent stops auto-compacting.
    #[serde(default = "default_max_consecutive_failures")]
    pub max_consecutive_failures: u8,

    /// Maximum prompt-too-long retries. Each retry drops a chunk of
    /// the oldest history before re-issuing the summarize request.
    #[serde(default = "default_max_ptl_retries")]
    pub max_ptl_retries: u8,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            trigger_buffer_pct: default_trigger_buffer_pct(),
            summary_output_reserve_pct: default_summary_output_reserve_pct(),
            preserve_min_pct: default_preserve_min_pct(),
            preserve_max_pct: default_preserve_max_pct(),
            preserve_min_text_messages: default_preserve_min_text_messages(),
            max_consecutive_failures: default_max_consecutive_failures(),
            max_ptl_retries: default_max_ptl_retries(),
        }
    }
}

fn default_trigger_buffer_pct() -> u8 {
    DEFAULT_TRIGGER_BUFFER_PCT
}
fn default_summary_output_reserve_pct() -> u8 {
    DEFAULT_SUMMARY_OUTPUT_RESERVE_PCT
}
fn default_preserve_min_pct() -> u8 {
    DEFAULT_PRESERVE_MIN_PCT
}
fn default_preserve_max_pct() -> u8 {
    DEFAULT_PRESERVE_MAX_PCT
}
fn default_preserve_min_text_messages() -> usize {
    DEFAULT_PRESERVE_MIN_TEXT_MESSAGES
}
fn default_max_consecutive_failures() -> u8 {
    DEFAULT_MAX_CONSECUTIVE_FAILURES
}
fn default_max_ptl_retries() -> u8 {
    DEFAULT_MAX_PTL_RETRIES
}

/// Window-relative budgets resolved from a [`CompactConfig`] +
/// concrete [`LLMProfile`]. Centralized so partition / trigger logic
/// can share the same arithmetic without re-deriving it.
#[derive(Debug, Clone, Copy)]
pub struct CompactBudgets {
    /// `max_context_window * (1 - summary_output_reserve_pct/100)`.
    pub effective_window: u32,
    /// `effective_window * (1 - trigger_buffer_pct/100)` — auto
    /// compact fires above this.
    pub trigger_threshold: u32,
    /// Resolved `preserve_min_pct` of `max_context_window`.
    pub preserve_min_tokens: u32,
    /// Resolved `preserve_max_pct` of `max_context_window`.
    pub preserve_max_tokens: u32,
    /// Resolved `summary_output_reserve_pct` of `max_context_window` —
    /// used as the `max_tokens` cap on the summary call.
    pub summary_output_tokens: u32,
}

impl CompactBudgets {
    #[must_use]
    pub fn resolve(profile: &LLMProfile, cfg: &CompactConfig) -> Self {
        let window = profile.max_context_window;
        let summary_output = pct_of(window, cfg.summary_output_reserve_pct);
        let effective = window.saturating_sub(summary_output);
        let trigger_threshold = effective.saturating_sub(pct_of(effective, cfg.trigger_buffer_pct));
        Self {
            effective_window: effective,
            trigger_threshold,
            preserve_min_tokens: pct_of(window, cfg.preserve_min_pct),
            preserve_max_tokens: pct_of(window, cfg.preserve_max_pct),
            summary_output_tokens: summary_output,
        }
    }
}

/// `window * pct / 100`, saturating on overflow. `pct` outside
/// `0..=100` is clamped to `100` — invalid config degrades to "full
/// window", a noisy but non-panicking failure mode.
fn pct_of(window: u32, pct: u8) -> u32 {
    let p = u64::from(pct.min(100));
    let w = u64::from(window);
    u32::try_from(w.saturating_mul(p) / 100).unwrap_or(u32::MAX)
}

/// Runtime state shared across compact invocations on a single
/// agent instance. Tracks the circuit-breaker counter so a series
/// of failed compactions doesn't burn tokens forever.
#[derive(Debug, Default)]
pub struct CompactState {
    /// Consecutive failures since the last successful compact.
    /// Reset to zero on any `Ok(_)` from [`compact_messages`].
    pub consecutive_failures: u8,
}

impl CompactState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when the breaker is open — the caller should stop
    /// auto-triggering and let the user know.
    #[must_use]
    pub fn is_circuit_open(&self, cfg: &CompactConfig) -> bool {
        self.consecutive_failures >= cfg.max_consecutive_failures
    }
}

/// Outcome of a successful run.
#[derive(Debug, Clone)]
pub struct CompactReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub estimated_tokens_before: u32,
    pub estimated_tokens_after: u32,
    pub trigger: CompactTrigger,
}

/// Errors surfaced by [`compact_messages`].
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    /// Circuit breaker is open — too many consecutive failures.
    /// The agent should stop auto-triggering and prompt the user.
    #[error("compact circuit breaker open ({0} consecutive failures)")]
    CircuitOpen(u8),
    /// Summary call kept hitting prompt-too-long even after PTL
    /// fallback retries.
    #[error("compact summary kept hitting prompt-too-long after {0} retries")]
    PtlExhausted(u8),
    /// Partition produced an empty compact region — the preserve
    /// region already covers everything.
    #[error("nothing to compact: preserve region already covers all messages")]
    NothingToCompact,
    /// Underlying LLM call failed.
    #[error(transparent)]
    Llm(#[from] crate::llm::Error),
}

/// Token estimate for a single message — char count divided by
/// `CHARS_PER_TOKEN`.
#[must_use]
pub fn estimate_tokens(msg: &Message) -> u32 {
    let chars = match msg {
        Message::System { content } | Message::User { content } | Message::Tool { content, .. } => {
            content.chars().count()
        }
        Message::Assistant {
            content,
            tool_calls,
            reasoning,
        } => {
            let mut total = content.as_deref().map_or(0, |s| s.chars().count());
            total += reasoning.as_deref().map_or(0, |s| s.chars().count());
            if let Some(calls) = tool_calls {
                for c in calls {
                    total += c.name.chars().count();
                    total += c.arguments.chars().count();
                }
            }
            total
        }
        Message::Compact(boundary) => boundary.summary.chars().count(),
    };
    u32::try_from(chars / CHARS_PER_TOKEN).unwrap_or(u32::MAX)
}

/// Sum [`estimate_tokens`] over a slice of messages, saturating on
/// overflow.
#[must_use]
pub fn estimate_total_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .map(estimate_tokens)
        .fold(0u32, u32::saturating_add)
}

/// Decision helper — `true` when the agent should run a compaction
/// before its next LLM call. Cheap (just `estimate_total_tokens`).
#[must_use]
pub fn should_compact(messages: &[Message], profile: &LLMProfile, cfg: &CompactConfig) -> bool {
    let budgets = CompactBudgets::resolve(profile, cfg);
    estimate_total_tokens(messages) > budgets.trigger_threshold
}

/// Run the compact pipeline.
///
/// Manual `/compact` and the auto-trigger path both call this. The
/// function is pure with respect to disk: callers persist the
/// returned message list themselves, typically by appending a compact
/// event through `crate::session::Manager::append_compaction`.
///
/// State re-injection (Claude Code's
/// `createPostCompactFileAttachments` and friends) isn't wired here
/// because we don't yet track active file reads / plans / skills.
/// When that lands the workspace path will come from
/// [`crate::config::AppConfig::data_dir`] — no new parameter on this
/// function is needed.
///
/// # Errors
///
/// See [`CompactError`]. On any error path the circuit-breaker
/// counter is incremented; on success it resets to zero.
#[allow(clippy::too_many_arguments)] // staying explicit while the shape settles.
pub async fn compact_messages(
    messages: Vec<Message>,
    profile: &LLMProfile,
    client: &dyn BaseLLMClient,
    cfg: &CompactConfig,
    state: &mut CompactState,
    trigger: CompactTrigger,
    summary_system_prompt: &str,
    timeout_secs: Option<u64>,
) -> Result<(Vec<Message>, CompactReport), CompactError> {
    if state.is_circuit_open(cfg) {
        return Err(CompactError::CircuitOpen(state.consecutive_failures));
    }

    let budgets = CompactBudgets::resolve(profile, cfg);
    let messages_before = messages.len();
    let estimated_tokens_before = estimate_total_tokens(&messages);

    // Split off sticky `system` messages so partition operates on
    // the conversation body only — Claude Code's compact also keeps
    // the system prompt in place across boundaries.
    let (sticky_system, body): (Vec<Message>, Vec<Message>) = messages
        .into_iter()
        .partition(|m| matches!(m, Message::System { .. }));

    let preserve_start = partition_preserve_start(&body, &budgets, cfg);
    let preserve: Vec<Message> = body[preserve_start..].to_vec();
    let mut compact_region: Vec<Message> = body[..preserve_start].to_vec();
    if compact_region.is_empty() {
        return Err(CompactError::NothingToCompact);
    }
    let messages_summarized = compact_region.len();
    let pre_compact_tokens = estimate_total_tokens(&compact_region);

    let summary = match summarize_with_ptl_fallback(
        &mut compact_region,
        profile,
        client,
        cfg,
        &budgets,
        summary_system_prompt,
        timeout_secs,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            return Err(e);
        }
    };

    let boundary = build_boundary(summary, trigger, pre_compact_tokens, messages_summarized);
    let mut out = sticky_system;
    out.push(boundary);
    out.extend(preserve);

    state.consecutive_failures = 0;
    let estimated_tokens_after = estimate_total_tokens(&out);
    let messages_after = out.len();

    Ok((
        out,
        CompactReport {
            messages_before,
            messages_after,
            estimated_tokens_before,
            estimated_tokens_after,
            trigger,
        },
    ))
}

/// Partition the conversation body (system messages already split
/// out) and return the index where the preserve region starts.
///
/// Walks from the tail backwards, accumulating tokens and text-message
/// counts. Stops once both `preserve_min_tokens` and
/// `preserve_min_text_messages` are met, or sooner if adding the
/// next message would breach `preserve_max_tokens`.
///
/// Boundary alignment: the preserve region's first message must not
/// be a `Tool` reply — providers require it to immediately follow an
/// assistant message that emitted the matching `tool_calls`. We
/// adjust by walking the start index backwards through any leading
/// `Tool` messages.
fn partition_preserve_start(
    body: &[Message],
    budgets: &CompactBudgets,
    cfg: &CompactConfig,
) -> usize {
    if body.is_empty() {
        return 0;
    }

    let mut tokens = 0u32;
    let mut text_msgs = 0usize;
    let mut start = body.len();

    for i in (0..body.len()).rev() {
        let cost = estimate_tokens(&body[i]);
        // Adding this message would breach the hard cap — stop.
        if tokens.saturating_add(cost) > budgets.preserve_max_tokens {
            break;
        }
        // Both lower bounds satisfied — stop before adding more.
        if tokens >= budgets.preserve_min_tokens && text_msgs >= cfg.preserve_min_text_messages {
            break;
        }
        tokens = tokens.saturating_add(cost);
        start = i;
        if is_text_message(&body[i]) {
            text_msgs += 1;
        }
    }

    // Boundary alignment: pull leading `Tool` messages back into the
    // preserve region by extending start backwards. Otherwise the
    // model would see a `Tool` reply with no matching tool_calls.
    while start < body.len() && matches!(&body[start], Message::Tool { .. }) {
        if start == 0 {
            break;
        }
        start -= 1;
    }

    start
}

/// "Counts as conversation" for the partition's text-message floor.
/// User text and assistant text both count; tool exchanges and
/// existing compact boundaries do not.
fn is_text_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::User { .. }
            | Message::Assistant {
                content: Some(_),
                ..
            }
    )
}

/// Summarize `compact_region`, retrying with progressive head
/// truncation when the request itself returns prompt-too-long.
///
/// Mirrors Claude Code's `truncateHeadForPTLRetry` — each retry
/// drops the oldest 20% of messages and re-issues the call. After
/// `max_ptl_retries` retries we surface [`CompactError::PtlExhausted`].
async fn summarize_with_ptl_fallback(
    compact_region: &mut Vec<Message>,
    profile: &LLMProfile,
    client: &dyn BaseLLMClient,
    cfg: &CompactConfig,
    budgets: &CompactBudgets,
    summary_system_prompt: &str,
    timeout_secs: Option<u64>,
) -> Result<String, CompactError> {
    let mut ptl_attempts: u8 = 0;
    loop {
        let request = build_summary_request(
            compact_region,
            profile,
            budgets,
            summary_system_prompt,
            timeout_secs,
        );
        match client.complete(request).await {
            Ok(resp) => {
                let summary = resp.content.unwrap_or_default().trim().to_string();
                if summary.is_empty() {
                    // Treat empty summary as a hard failure — caller
                    // bumps the circuit breaker. Better than writing
                    // an empty boundary that silently loses history.
                    return Err(CompactError::Llm(crate::llm::Error::Api {
                        status: 0,
                        body: "compact summary call returned empty content".into(),
                    }));
                }
                return Ok(summary);
            }
            Err(e) if is_ptl_error(&e) => {
                if ptl_attempts >= cfg.max_ptl_retries {
                    return Err(CompactError::PtlExhausted(ptl_attempts));
                }
                ptl_attempts = ptl_attempts.saturating_add(1);
                truncate_head_for_ptl_retry(compact_region);
                if compact_region.is_empty() {
                    return Err(CompactError::PtlExhausted(ptl_attempts));
                }
            }
            Err(e) => return Err(CompactError::Llm(e)),
        }
    }
}

/// Drop the oldest 20% of messages (at least one) from
/// `compact_region`. Mirrors Claude Code's PTL-fallback heuristic.
fn truncate_head_for_ptl_retry(compact_region: &mut Vec<Message>) {
    let drop = (compact_region.len() / 5).max(1);
    compact_region.drain(..drop.min(compact_region.len()));
}

/// Heuristic recognizer for prompt-too-long errors across providers.
/// Mistral / `DeepSeek` both return HTTP 400 with a body string that
/// mentions the limit; we keyword-match rather than parsing each
/// provider's error format.
fn is_ptl_error(err: &crate::llm::Error) -> bool {
    let crate::llm::Error::Api { body, .. } = err else {
        return false;
    };
    let lower = body.to_lowercase();
    let is_prompt_or_context = lower.contains("prompt") || lower.contains("context");
    let is_overflow = lower.contains("too long")
        || lower.contains("too large")
        || lower.contains("exceed")
        || lower.contains("maximum");
    is_prompt_or_context && is_overflow
}

/// Build the LLM request that produces the summary. The summary
/// call is non-streaming; thinking is explicitly disabled so even a
/// reasoning-default model returns just the summary text.
fn build_summary_request(
    compact_region: &[Message],
    profile: &LLMProfile,
    budgets: &CompactBudgets,
    summary_system_prompt: &str,
    timeout_secs: Option<u64>,
) -> Request {
    let dump = format_messages_as_dump(compact_region);

    Request {
        messages: vec![
            Message::System {
                content: summary_system_prompt.to_string(),
            },
            Message::User { content: dump },
        ],
        tools: Vec::new(),
        model_name: profile.model_name.clone(),
        max_tokens: Some(budgets.summary_output_tokens),
        temperature: None,
        timeout_secs,
        thinking: Some(Thinking {
            enabled: false,
            reasoning_effort: None,
        }),
    }
}

/// Render a slice of [`Message`]s as a plain-text transcript the
/// summary LLM can read. Tool calls and tool results get explicit
/// labels so the model understands the structure even though the
/// dump is delivered as a single user message.
fn format_messages_as_dump(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            Message::System { content } => {
                out.push_str("[system] ");
                out.push_str(content);
                out.push('\n');
            }
            Message::User { content } => {
                out.push_str("[user] ");
                out.push_str(content);
                out.push('\n');
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if let Some(text) = content {
                    out.push_str("[assistant] ");
                    out.push_str(text);
                    out.push('\n');
                }
                if let Some(calls) = tool_calls {
                    for c in calls {
                        out.push_str("[assistant→tool_call: ");
                        out.push_str(&c.name);
                        out.push_str("] ");
                        out.push_str(&c.arguments);
                        out.push('\n');
                    }
                }
            }
            Message::Tool {
                content,
                tool_call_id,
            } => {
                out.push_str("[tool result for ");
                out.push_str(tool_call_id);
                out.push_str("] ");
                out.push_str(content);
                out.push('\n');
            }
            Message::Compact(b) => {
                out.push_str("[earlier summary] ");
                out.push_str(&b.summary);
                out.push('\n');
            }
        }
    }
    out
}

/// Build a [`Message::Compact`] for the given summary text. Kept as
/// a freestanding helper so the agent loop can construct the
/// boundary without reaching into `crate::llm::types` directly.
#[must_use]
pub fn build_boundary(
    summary: String,
    trigger: CompactTrigger,
    pre_tokens: u32,
    messages_summarized: usize,
) -> Message {
    Message::Compact(CompactBoundary {
        summary,
        trigger,
        pre_tokens,
        messages_summarized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(s: &str) -> Message {
        Message::User { content: s.into() }
    }

    fn profile(window: u32) -> LLMProfile {
        LLMProfile {
            model_name: "x".into(),
            max_context_window: window,
            max_tokens: None,
            temperature: None,
            thinking: None,
        }
    }

    #[test]
    fn estimate_tokens_uses_char_count_divided_by_four() {
        // 8 chars / 4 = 2 tokens
        assert_eq!(estimate_tokens(&user("12345678")), 2);
        // sub-divisor counts round down
        assert_eq!(estimate_tokens(&user("123")), 0);
    }

    #[test]
    fn estimate_tokens_assistant_sums_content_reasoning_and_tool_calls() {
        let msg = Message::Assistant {
            content: Some("hello".into()),   // 5
            reasoning: Some("trace".into()), // 5
            tool_calls: Some(vec![crate::llm::ToolCall {
                // 4 + 4 = 8
                id: "id".into(),
                name: "tool".into(),
                arguments: "args".into(),
            }]),
        };
        // (5 + 5 + 8) / 4 = 18/4 = 4
        assert_eq!(estimate_tokens(&msg), 4);
    }

    #[test]
    fn pct_of_clamps_invalid_input_to_full_window() {
        assert_eq!(pct_of(1000, 0), 0);
        assert_eq!(pct_of(1000, 50), 500);
        assert_eq!(pct_of(1000, 100), 1000);
        // Out-of-range clamps to 100% rather than panicking.
        assert_eq!(pct_of(1000, 200), 1000);
    }

    #[test]
    fn budgets_scale_with_window() {
        let cfg = CompactConfig::default();
        let small = CompactBudgets::resolve(&profile(200_000), &cfg);
        let large = CompactBudgets::resolve(&profile(1_000_000), &cfg);
        // Same percentages, different absolute values — exactly the
        // motivation for moving away from fixed token counts.
        assert!(large.effective_window > small.effective_window * 4);
        assert!(large.preserve_max_tokens > small.preserve_max_tokens * 4);
    }

    #[test]
    fn should_compact_fires_above_threshold() {
        // 0% summary reserve, 0% trigger buffer => threshold = full window.
        let cfg = CompactConfig {
            summary_output_reserve_pct: 0,
            trigger_buffer_pct: 0,
            ..CompactConfig::default()
        };
        let prof = profile(1_000);
        let small = vec![user(&"a".repeat(800))]; // 200 tokens
        assert!(!should_compact(&small, &prof, &cfg));
        let big = vec![user(&"a".repeat(8_000))]; // 2000 tokens
        assert!(should_compact(&big, &prof, &cfg));
    }

    #[test]
    fn circuit_breaker_opens_at_threshold() {
        let cfg = CompactConfig::default();
        let mut state = CompactState::new();
        assert!(!state.is_circuit_open(&cfg));
        state.consecutive_failures = cfg.max_consecutive_failures;
        assert!(state.is_circuit_open(&cfg));
    }

    #[test]
    fn build_boundary_round_trips_through_message_compact() {
        let msg = build_boundary("summary".into(), CompactTrigger::Manual, 1234, 5);
        match msg {
            Message::Compact(boundary) => {
                assert_eq!(boundary.summary, "summary");
                assert_eq!(boundary.trigger, CompactTrigger::Manual);
                assert_eq!(boundary.pre_tokens, 1234);
                assert_eq!(boundary.messages_summarized, 5);
            }
            _ => panic!("expected Message::Compact"),
        }
    }

    fn assistant_text(s: &str) -> Message {
        Message::Assistant {
            content: Some(s.into()),
            tool_calls: None,
            reasoning: None,
        }
    }

    fn assistant_call(id: &str, name: &str, args: &str) -> Message {
        Message::Assistant {
            content: None,
            reasoning: None,
            tool_calls: Some(vec![crate::llm::ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.into(),
            }]),
        }
    }

    fn tool_result(id: &str, content: &str) -> Message {
        Message::Tool {
            content: content.into(),
            tool_call_id: id.into(),
        }
    }

    /// Easy helper: ten short user/assistant rounds, plenty of text
    /// messages so the partition lands purely on token budget.
    fn ten_rounds() -> Vec<Message> {
        let mut v = Vec::new();
        for i in 0..10 {
            v.push(user(&format!("question-{i:02}: {}", "x".repeat(40))));
            v.push(assistant_text(&format!(
                "answer-{i:02}: {}",
                "y".repeat(40)
            )));
        }
        v
    }

    #[test]
    fn partition_respects_min_text_messages_floor() {
        let body = ten_rounds();
        // Min text messages forces preservation of at least 8 text
        // messages even though the token floor is tiny.
        let cfg = CompactConfig {
            preserve_min_text_messages: 8,
            ..CompactConfig::default()
        };
        let budgets = CompactBudgets::resolve(&profile(1_000_000), &cfg);
        let start = partition_preserve_start(&body, &budgets, &cfg);
        // 8 text messages of 40+chars ≈ small but positive token
        // count; preserve region length should be ≥ 8.
        assert!(body.len() - start >= 8);
    }

    #[test]
    fn partition_pulls_tool_replies_back_into_preserve() {
        // Sequence ends with a tool-call/tool-result pair; if the
        // partition would otherwise start at the Tool message, the
        // alignment step must walk it back to include the Assistant
        // tool_call message that produced it.
        let body = vec![
            user("hello"),
            assistant_text("first"),
            user("do work"),
            assistant_call("call_1", "search", r#"{"q":"x"}"#),
            tool_result("call_1", "result text"),
            assistant_text("done"),
        ];
        // Force the preserve window very small so the natural cut
        // wants to land mid-pair.
        let cfg = CompactConfig {
            preserve_min_pct: 0,
            preserve_max_pct: 100,
            preserve_min_text_messages: 1,
            ..CompactConfig::default()
        };
        let budgets = CompactBudgets::resolve(&profile(10_000), &cfg);
        let start = partition_preserve_start(&body, &budgets, &cfg);
        assert!(
            !matches!(body.get(start), Some(Message::Tool { .. })),
            "preserve region must not start with a Tool message; started at index {start}"
        );
    }

    #[test]
    fn truncate_head_drops_at_least_one_and_about_a_fifth() {
        let mut v: Vec<Message> = (0..10).map(|i| user(&format!("{i}"))).collect();
        truncate_head_for_ptl_retry(&mut v);
        // 10 / 5 = 2 dropped → 8 left.
        assert_eq!(v.len(), 8);

        let mut v: Vec<Message> = vec![user("only")];
        truncate_head_for_ptl_retry(&mut v);
        // Single-element input — drop at least one → empty.
        assert!(v.is_empty());
    }

    #[test]
    fn ptl_recognizer_matches_common_phrasings() {
        let mk = |body: &str| crate::llm::Error::Api {
            status: 400,
            body: body.into(),
        };
        assert!(is_ptl_error(&mk("Prompt is too long")));
        assert!(is_ptl_error(&mk("context length exceeded")));
        assert!(is_ptl_error(&mk("Maximum context window")));
        assert!(!is_ptl_error(&mk("invalid api key")));
        assert!(!is_ptl_error(&mk("rate limit")));
    }

    #[test]
    fn dump_renders_each_message_with_role_label() {
        let body = vec![
            user("hi"),
            assistant_text("hello"),
            assistant_call("c1", "t", "{}"),
            tool_result("c1", "ok"),
        ];
        let dump = format_messages_as_dump(&body);
        assert!(dump.contains("[user] hi"));
        assert!(dump.contains("[assistant] hello"));
        assert!(dump.contains("[assistant→tool_call: t]"));
        assert!(dump.contains("[tool result for c1]"));
    }
}
