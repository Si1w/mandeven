//! Short model-facing identifiers for durable domain objects.
//!
//! Sessions, transcript messages, and execution logs keep UUIDs because
//! they need global uniqueness across resume/sync boundaries. Tasks and
//! timers are referenced in prompts and CLI output, so they use short
//! prefixed IDs in the same spirit as Claude Code's background task IDs.

use uuid::Uuid;

const ID_LEN: usize = 8;
const TASK_PREFIX: char = 't';
const TIMER_PREFIX: char = 'r';
const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Generate a task id such as `t9f3k2p1q`.
#[must_use]
pub fn new_task_id() -> String {
    short_id(TASK_PREFIX)
}

/// Generate a timer id such as `r4h8x1k2m`.
#[must_use]
pub fn new_timer_id() -> String {
    short_id(TIMER_PREFIX)
}

/// Validate the current task id shape.
#[must_use]
pub fn is_task_id(raw: &str) -> bool {
    is_prefixed_short_id(raw, TASK_PREFIX)
}

/// Validate the current timer id shape.
#[must_use]
pub fn is_timer_id(raw: &str) -> bool {
    is_prefixed_short_id(raw, TIMER_PREFIX)
}

fn short_id(prefix: char) -> String {
    let mut value = Uuid::now_v7().as_u128();
    let mut out = String::with_capacity(ID_LEN + 1);
    out.push(prefix);
    for _ in 0..ID_LEN {
        let idx = usize::try_from(value % 36).expect("modulo 36 always fits usize");
        out.push(char::from(ALPHABET[idx]));
        value /= 36;
    }
    out
}

fn is_prefixed_short_id(raw: &str, prefix: char) -> bool {
    let mut chars = raw.chars();
    chars.next() == Some(prefix)
        && chars.clone().count() == ID_LEN
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_are_short_and_prefixed() {
        let id = new_task_id();
        assert!(is_task_id(&id));
        assert!(!is_timer_id(&id));
    }

    #[test]
    fn timer_ids_are_short_and_prefixed() {
        let id = new_timer_id();
        assert!(is_timer_id(&id));
        assert!(!is_task_id(&id));
    }
}
