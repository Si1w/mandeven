//! Message chunking shared by IM channels with per-message length
//! limits (Discord 2000, Telegram 4096, Slack ~40 000 …).
//!
//! Strategy: walk the source text and pack as many newline-terminated
//! lines into the current chunk as fit. When a single line exceeds the
//! limit on its own, fall back to a hard character-boundary split so
//! the function never produces a chunk larger than `max_chars`.
//!
//! Operates on Unicode scalar value counts, not bytes — Discord's
//! "2000 characters" limit counts UCS-4 code points, not UTF-8 bytes.

/// Split `text` into chunks each of at most `max_chars` Unicode scalar
/// values, preferring newline boundaries.
///
/// - Empty input returns an empty vector (caller skips sending).
/// - `max_chars == 0` is treated as "no chunking possible" and returns
///   the input as a single chunk; callers should guard against that
///   misuse — in practice every IM platform has a positive limit.
#[must_use]
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if max_chars == 0 || text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len: usize = 0;

    for line in text.split_inclusive('\n') {
        let line_len = line.chars().count();
        if line_len > max_chars {
            // Flush whatever we've accumulated so the oversize line
            // starts on a clean chunk boundary.
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            for ch in line.chars() {
                if current_len + 1 > max_chars {
                    chunks.push(std::mem::take(&mut current));
                    current_len = 0;
                }
                current.push(ch);
                current_len += 1;
            }
        } else if current_len + line_len > max_chars {
            chunks.push(std::mem::take(&mut current));
            current.push_str(line);
            current_len = line_len;
        } else {
            current.push_str(line);
            current_len += line_len;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::split_message;

    #[test]
    fn empty_input_returns_empty_vec() {
        assert!(split_message("", 100).is_empty());
    }

    #[test]
    fn short_input_returns_single_chunk() {
        assert_eq!(split_message("hello", 100), vec!["hello"]);
    }

    #[test]
    fn breaks_on_newline_when_over_limit() {
        let text = "line one\nline two\nline three";
        let chunks = split_message(text, 18);
        assert_eq!(chunks, vec!["line one\nline two\n", "line three"]);
    }

    #[test]
    fn hard_splits_when_a_single_line_exceeds_limit() {
        let text = "abcdefghij";
        assert_eq!(split_message(text, 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn counts_unicode_scalar_values_not_bytes() {
        // Each "中" is one scalar value but three UTF-8 bytes.
        let text = "中文测试一二三四";
        let chunks = split_message(text, 4);
        assert_eq!(chunks, vec!["中文测试", "一二三四"]);
    }
}
