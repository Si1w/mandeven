//! Throttled `send-then-edit` streaming state machine, reused by
//! every IM channel that supports message editing.
//!
//! Pattern (mirrors nanobot's `_StreamBuf` from
//! `agent-examples/nanobot/nanobot/channels/discord.py`):
//!
//! 1. First non-empty delta → emit one message and remember it.
//! 2. Subsequent deltas → append to the buffer; only emit an edit when
//!    `edit_interval` has elapsed since the last emit. This keeps the
//!    channel under platform rate limits (Discord allows roughly five
//!    edits per five seconds per channel).
//! 3. End-of-stream → produce a final edit covering everything that
//!    accumulated since the last emit, plus any overflow chunks that
//!    would exceed the platform's per-message length limit.
//!
//! The state machine is platform-agnostic. It does not hold platform
//! handles (Discord `MessageId`, Slack `ts`, …); callers map
//! [`StreamAction`] decisions to their own send / edit calls and
//! remember the message handle on their side.

use std::time::{Duration, Instant};

use super::chunk::split_message;

/// What the caller should do after appending a delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamAction {
    /// No platform call. The buffer absorbed the delta silently.
    Buffer,
    /// Send a fresh message containing this content. The caller stores
    /// the resulting message handle and uses it for every subsequent
    /// [`StreamAction::Edit`].
    SendInitial(String),
    /// Edit the already-sent message to match this content.
    Edit(String),
}

/// Final state after [`StreamBuf::finalize`].
///
/// `head` is the content the caller must apply with one last edit on
/// the original message; `tail` is any further chunks that didn't fit
/// the platform's per-message length limit and must be sent as new
/// messages, in order.
#[derive(Debug, Clone)]
pub struct FinalizeResult {
    /// Final content for the initial message. `None` when the buffer
    /// never emitted a [`StreamAction::SendInitial`] — typically a
    /// stream that produced only whitespace.
    pub head: Option<String>,
    /// Overflow chunks to send as fresh messages.
    pub tail: Vec<String>,
}

/// Per-stream throttled accumulator.
pub struct StreamBuf {
    text: String,
    started: bool,
    last_io_at: Option<Instant>,
    edit_interval: Duration,
    max_chars: usize,
}

impl StreamBuf {
    /// Construct an empty buffer.
    ///
    /// `edit_interval` is the minimum delay between `Edit` actions —
    /// `Duration::from_secs(1)` is the safe default for Discord.
    /// `max_chars` is the platform's per-message scalar-value limit
    /// (Discord 2000, Telegram 4096, …); the buffer uses it to decide
    /// when to truncate the in-stream preview so a single edit never
    /// overflows.
    #[must_use]
    pub fn new(edit_interval: Duration, max_chars: usize) -> Self {
        Self {
            text: String::new(),
            started: false,
            last_io_at: None,
            edit_interval,
            max_chars,
        }
    }

    /// Append `delta` and decide what the caller should do next.
    pub fn append(&mut self, delta: &str) -> StreamAction {
        self.text.push_str(delta);
        if !self.started {
            // Hold off the initial send until we have something a user
            // would actually want to see — pure-whitespace deltas before
            // the model produces real text are common.
            if self.text.trim().is_empty() {
                return StreamAction::Buffer;
            }
            self.started = true;
            self.last_io_at = Some(Instant::now());
            return StreamAction::SendInitial(self.preview());
        }

        match self.last_io_at {
            Some(t) if t.elapsed() < self.edit_interval => StreamAction::Buffer,
            _ => {
                self.last_io_at = Some(Instant::now());
                StreamAction::Edit(self.preview())
            }
        }
    }

    /// End the stream and produce the final action plan.
    #[must_use]
    pub fn finalize(self) -> FinalizeResult {
        if !self.started {
            return FinalizeResult {
                head: None,
                tail: Vec::new(),
            };
        }
        let mut chunks = split_message(&self.text, self.max_chars).into_iter();
        FinalizeResult {
            head: chunks.next(),
            tail: chunks.collect(),
        }
    }

    fn preview(&self) -> String {
        // The streamed text could already exceed `max_chars`, but a
        // single Discord edit only accepts up to that limit; show the
        // first chunk during streaming and dump overflow on finalize.
        split_message(&self.text, self.max_chars)
            .into_iter()
            .next()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamAction, StreamBuf};
    use std::time::Duration;

    #[test]
    fn first_non_whitespace_delta_triggers_send_initial() {
        let mut buf = StreamBuf::new(Duration::from_millis(50), 2000);
        assert_eq!(buf.append("   "), StreamAction::Buffer);
        match buf.append("hi") {
            StreamAction::SendInitial(text) => assert_eq!(text, "   hi"),
            other => panic!("expected SendInitial, got {other:?}"),
        }
    }

    #[test]
    fn second_delta_within_interval_buffers() {
        let mut buf = StreamBuf::new(Duration::from_mins(1), 2000);
        let _ = buf.append("hello");
        assert_eq!(buf.append(" world"), StreamAction::Buffer);
    }

    #[test]
    fn delta_after_interval_emits_edit() {
        let mut buf = StreamBuf::new(Duration::from_millis(1), 2000);
        let _ = buf.append("hello");
        std::thread::sleep(Duration::from_millis(5));
        match buf.append(" world") {
            StreamAction::Edit(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Edit, got {other:?}"),
        }
    }

    #[test]
    fn finalize_returns_none_when_never_started() {
        let buf = StreamBuf::new(Duration::from_millis(50), 2000);
        let res = buf.finalize();
        assert!(res.head.is_none());
        assert!(res.tail.is_empty());
    }

    #[test]
    fn finalize_splits_overflow_into_tail() {
        let mut buf = StreamBuf::new(Duration::from_millis(50), 4);
        let _ = buf.append("abcdefghij");
        let res = buf.finalize();
        assert_eq!(res.head.as_deref(), Some("abcd"));
        assert_eq!(res.tail, vec!["efgh", "ij"]);
    }
}
