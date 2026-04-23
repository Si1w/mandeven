//! Filesystem tools — `file_read`, `file_write`, `file_edit`.
//!
//! Design notes:
//!
//! - **No sandbox**: paths are used as given; there is no workspace
//!   confinement. The agent operates with the host user's full
//!   filesystem privileges.
//! - **Device-path blocklist**: [`FileRead`] refuses anything starting
//!   with `/dev/` so the model cannot read `/dev/random` and fill
//!   memory (or block on `/dev/tty`).
//! - **UTF-8 only**: files are decoded as UTF-8; binary files produce
//!   a clean error rather than lossy bytes.
//! - **CRLF normalization**: input content has `\r\n` collapsed to
//!   `\n` before line counting and matching so downstream behavior is
//!   consistent regardless of the file's original line endings.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::error::{Error, Result};
use super::{BaseTool, MAX_TOOL_RESULT_BYTES};
use crate::llm::Tool;

/// Default number of lines returned by `file_read` when `limit` is
/// unset.
const DEFAULT_READ_LIMIT: usize = 2000;

// ---------------------------------------------------------------------------
// file_read
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ReadParams {
    /// File path, absolute or relative to the process CWD.
    path: String,
    /// 1-indexed line to start from. Defaults to `1`.
    #[serde(default)]
    offset: Option<usize>,
    /// Maximum lines to return. Defaults to `2000`.
    #[serde(default)]
    limit: Option<usize>,
}

/// Read a UTF-8 text file and return its contents with per-line line
/// numbers (`N| ...`).
pub struct FileRead;

#[async_trait]
impl BaseTool for FileRead {
    fn schema(&self) -> Tool {
        Tool {
            name: "file_read".into(),
            description: "Read a UTF-8 text file. Output format: LINE_NUM| CONTENT. \
                Supports line-range pagination via offset (1-indexed) and limit. \
                /dev/* paths are blocked. Binary files produce an error."
                .into(),
            parameters: serde_json::to_value(schema_for!(ReadParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let p: ReadParams = parse_params("file_read", args)?;
        if is_dev_path(&p.path) {
            return Err(exec("file_read", format!("reading {} is blocked", p.path)));
        }

        let raw = tokio::fs::read(&p.path)
            .await
            .map_err(|e| exec("file_read", format!("{}: {e}", p.path)))?;
        let Ok(content) = String::from_utf8(raw) else {
            return Err(exec(
                "file_read",
                format!("{} is not valid UTF-8 (binary file?)", p.path),
            ));
        };
        let content = content.replace("\r\n", "\n");

        let lines: Vec<&str> = content.split('\n').collect();
        // `split('\n')` on a trailing newline yields an extra empty
        // element; drop it so line counts match `wc -l`.
        let total = if content.ends_with('\n') && !lines.is_empty() {
            lines.len() - 1
        } else {
            lines.len()
        };
        if total == 0 {
            return Ok(Value::String(format!("(Empty file: {})", p.path)));
        }

        let offset = p.offset.unwrap_or(1).max(1);
        let limit = p.limit.unwrap_or(DEFAULT_READ_LIMIT);
        if offset > total {
            return Err(exec(
                "file_read",
                format!("offset {offset} is beyond end of file ({total} lines)"),
            ));
        }

        let start = offset - 1;
        let end = (start + limit).min(total);
        let mut out = String::new();
        let mut truncated = false;
        for (i, line) in lines[start..end].iter().enumerate() {
            let formatted = format!("{}| {}", start + i + 1, line);
            if out.len() + formatted.len() + 1 > MAX_TOOL_RESULT_BYTES {
                truncated = true;
                break;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&formatted);
        }

        if truncated {
            write!(
                out,
                "\n\n(truncated at ~{}KB — lower `limit` or increase `offset` to page further)",
                MAX_TOOL_RESULT_BYTES / 1000
            )
            .expect("writing to String is infallible");
        } else if end < total {
            write!(
                out,
                "\n\n(Showing lines {offset}-{end} of {total}. Use offset={} to continue.)",
                end + 1
            )
            .expect("writing to String is infallible");
        } else {
            write!(out, "\n\n(End of file — {total} lines total)")
                .expect("writing to String is infallible");
        }

        Ok(Value::String(out))
    }
}

// ---------------------------------------------------------------------------
// file_write
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct WriteParams {
    /// File path, absolute or relative to CWD. Parent directories are
    /// created if missing.
    path: String,
    /// Full file contents; overwrites any existing file.
    content: String,
}

/// Write a file, creating parent directories as needed. Overwrites
/// existing files without prompting.
pub struct FileWrite;

#[async_trait]
impl BaseTool for FileWrite {
    fn schema(&self) -> Tool {
        Tool {
            name: "file_write".into(),
            description: "Write content to a file. Overwrites if the file exists; \
                creates parent directories as needed. For partial edits, prefer \
                file_edit."
                .into(),
            parameters: serde_json::to_value(schema_for!(WriteParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let p: WriteParams = parse_params("file_write", args)?;
        let path = PathBuf::from(&p.path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| exec("file_write", format!("create parents of {}: {e}", p.path)))?;
        }
        tokio::fs::write(&path, &p.content)
            .await
            .map_err(|e| exec("file_write", format!("{}: {e}", p.path)))?;
        Ok(Value::String(format!(
            "Wrote {} bytes to {}",
            p.content.len(),
            p.path
        )))
    }
}

// ---------------------------------------------------------------------------
// file_edit
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct EditParams {
    /// Path to the file being edited.
    path: String,
    /// Exact text to find. Pass the empty string + non-existent file
    /// to request file creation with `new_string` as the new contents.
    old_string: String,
    /// Replacement text.
    new_string: String,
    /// When `false` (default), the edit fails if `old_string` matches
    /// more than once — use `replace_all` or add more context.
    #[serde(default)]
    replace_all: bool,
}

/// Byte-range span of one match in the source content.
#[derive(Debug, Clone, Copy)]
struct MatchSpan {
    start: usize,
    end: usize,
    /// 1-based line number where the match starts.
    line: usize,
}

/// Edit a file by substituting `old_string` with `new_string`. Two-tier
/// match: exact substring, then per-line trimmed sliding window.
pub struct FileEdit;

#[async_trait]
impl BaseTool for FileEdit {
    fn schema(&self) -> Tool {
        Tool {
            name: "file_edit".into(),
            description: "Edit a file by replacing old_string with new_string. \
                Tolerates per-line whitespace differences via a trimmed sliding \
                window match when the exact substring is not found. Fails if \
                old_string matches multiple times unless replace_all=true. \
                Special case: old_string=\"\" + non-existent file creates a new file."
                .into(),
            parameters: serde_json::to_value(schema_for!(EditParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let p: EditParams = parse_params("file_edit", args)?;
        let path = PathBuf::from(&p.path);

        // Create-file special case.
        if p.old_string.is_empty() && !path.exists() {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| exec("file_edit", format!("create parents of {}: {e}", p.path)))?;
            }
            tokio::fs::write(&path, &p.new_string)
                .await
                .map_err(|e| exec("file_edit", format!("{}: {e}", p.path)))?;
            return Ok(Value::String(format!("Created {}", p.path)));
        }

        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| exec("file_edit", format!("{}: {e}", p.path)))?;
        let source = String::from_utf8(raw)
            .map_err(|_| exec("file_edit", format!("{} is not valid UTF-8", p.path)))?;
        let uses_crlf = source.contains("\r\n");
        let content = source.replace("\r\n", "\n");
        let old = p.old_string.replace("\r\n", "\n");
        let new = p.new_string.replace("\r\n", "\n");

        // Tier 1: exact substring match.
        let mut matches = find_exact(&content, &old);
        // Tier 2: line-trimmed sliding window.
        if matches.is_empty() {
            matches = find_trim(&content, &old);
        }
        if matches.is_empty() {
            return Err(exec(
                "file_edit",
                format!("old_string not found in {}", p.path),
            ));
        }
        if matches.len() > 1 && !p.replace_all {
            let lines: Vec<String> = matches
                .iter()
                .take(3)
                .map(|m| format!("line {}", m.line))
                .collect();
            let suffix = if matches.len() > 3 { ", ..." } else { "" };
            return Err(exec(
                "file_edit",
                format!(
                    "old_string matches {} times at {}{}. Add more context or set replace_all=true.",
                    matches.len(),
                    lines.join(", "),
                    suffix
                ),
            ));
        }

        let selected = if p.replace_all {
            matches.as_slice()
        } else {
            &matches[..1]
        };
        let mut result = content.clone();
        // Apply from the back so earlier offsets stay valid.
        for m in selected.iter().rev() {
            result.replace_range(m.start..m.end, &new);
        }
        let final_content = if uses_crlf {
            result.replace('\n', "\r\n")
        } else {
            result
        };
        tokio::fs::write(&path, &final_content)
            .await
            .map_err(|e| exec("file_edit", format!("{}: {e}", p.path)))?;
        Ok(Value::String(format!(
            "Replaced {} occurrence(s) in {}",
            selected.len(),
            p.path
        )))
    }
}

/// Exact substring search. Matches are non-overlapping.
fn find_exact(content: &str, needle: &str) -> Vec<MatchSpan> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(idx) = content[start..].find(needle) {
        let abs = start + idx;
        out.push(MatchSpan {
            start: abs,
            end: abs + needle.len(),
            line: content[..abs].matches('\n').count() + 1,
        });
        start = abs + needle.len();
    }
    out
}

/// Line-trimmed sliding window match. Splits both inputs by `\n`,
/// `trim()`-s each line, and returns the byte range covering any
/// window in `content` whose trimmed lines match `needle`'s trimmed
/// lines exactly.
fn find_trim(content: &str, needle: &str) -> Vec<MatchSpan> {
    let needle_lines: Vec<&str> = needle.split('\n').collect();
    if needle_lines.is_empty() {
        return Vec::new();
    }

    // Split content preserving newline offsets.
    let mut lines: Vec<&str> = Vec::new();
    let mut line_offsets: Vec<usize> = vec![0];
    let mut cursor = 0;
    for line in content.split_inclusive('\n') {
        // Strip the trailing '\n' from the line text, but keep offsets.
        let text = line.strip_suffix('\n').unwrap_or(line);
        lines.push(text);
        cursor += line.len();
        line_offsets.push(cursor);
    }
    if lines.len() < needle_lines.len() {
        return Vec::new();
    }

    let trimmed_needle: Vec<&str> = needle_lines.iter().map(|l| l.trim()).collect();
    let window = trimmed_needle.len();

    let mut out = Vec::new();
    let mut i = 0;
    while i + window <= lines.len() {
        let actual: Vec<&str> = lines[i..i + window].iter().map(|l| l.trim()).collect();
        if actual == trimmed_needle {
            let start = line_offsets[i];
            let mut end = line_offsets[i + window];
            // Drop the terminating newline from the match span so the
            // replacement slots in at the same position as an exact
            // match would.
            if content.as_bytes().get(end.saturating_sub(1)) == Some(&b'\n') {
                end -= 1;
            }
            out.push(MatchSpan {
                start,
                end,
                line: i + 1,
            });
            i += window;
        } else {
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn parse_params<T: for<'de> Deserialize<'de>>(tool: &'static str, args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|source| Error::InvalidArguments {
        tool: tool.to_string(),
        source,
    })
}

fn exec(tool: &'static str, message: impl Into<String>) -> Error {
    Error::Execution {
        tool: tool.to_string(),
        message: message.into(),
    }
}

/// Returns true when `path` references a device file we refuse to
/// read. Literal string match — does not resolve symlinks, so paths
/// like `/tmp/link-to-dev-null` are not caught (acceptable trade-off
/// for the minimal set this layer is defending against).
fn is_dev_path(path: &str) -> bool {
    Path::new(path).starts_with("/dev")
}
