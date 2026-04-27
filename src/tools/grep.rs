//! `grep` tool — search file contents via the `rg` (ripgrep) binary.
//!
//! Design notes:
//!
//! - **Shell out, not crate**: invokes the system `rg` binary directly via
//!   `tokio::process::Command`. Trades a runtime dependency on ripgrep for
//!   ~0 compile-time cost, full ripgrep feature surface, and zero re-implemented
//!   gitignore/encoding/binary-detection logic. Mirrors the approach codex
//!   used in its (now-removed) `grep_files` handler.
//! - **Read access is unbounded**: the `path` argument can point anywhere
//!   on disk, mirroring `file_read`. The 30s timeout, the byte cap, and
//!   the `head_limit` line cap together provide enough backstop against
//!   accidental full-disk scans without forcing the model to copy files
//!   into the workspace just to inspect them.
//! - **No raw flag passthrough**: the model fills enumerated parameters
//!   (`pattern`, `glob`, `case_insensitive`, …); this tool builds the argv.
//!   Closes the door on hostile flags like `--pre <cmd>` (arbitrary execution)
//!   or `--search-zip` (out-of-tree decompressors).
//! - **Three output modes**: `files_with_matches` (default, terse — same as
//!   codex), `content` (rg's default `path:line:text` with line numbers),
//!   `count` (`path:N` per file). Models that need source context ask for
//!   `content` explicitly; the default keeps token cost low.
//! - **Bounded output**: combined stdout is truncated at the shared
//!   [`super::MAX_TOOL_RESULT_BYTES`]. Optional `head_limit` lets the model
//!   cap line count before truncation kicks in.
//! - **Exit code semantics**: rg returns `0` on match, `1` on no-match,
//!   `>=2` on actual error. We treat `1` as an empty-but-successful result.

use std::fmt::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

use super::error::{Error, Result};
use super::{BaseTool, MAX_TOOL_RESULT_BYTES, ToolOutcome};
use crate::llm::Tool;
use crate::workspace;

/// Hard timeout on a single `rg` invocation. Mirrors codex's choice.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on the number of result lines kept from `rg`'s stdout when
/// the model omits `head_limit`. Set generously — the byte cap below is the
/// real backstop.
const DEFAULT_HEAD_LIMIT: usize = 500;

/// Hard upper bound on `head_limit`. Prevents the model from disabling the
/// safety net by passing `usize::MAX`.
const MAX_HEAD_LIMIT: usize = 10_000;

#[derive(Deserialize, JsonSchema)]
struct GrepParams {
    /// Regex pattern (Rust regex syntax — same dialect ripgrep uses).
    pattern: String,
    /// File or directory to search. Absolute or relative to CWD; can
    /// point anywhere on disk the user can read. Defaults to the
    /// workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Glob filter applied to file names, e.g. `"*.rs"` or `"src/**/*.ts"`.
    #[serde(default)]
    glob: Option<String>,
    /// Output shape. Defaults to `files_with_matches`.
    #[serde(default)]
    output_mode: Option<OutputMode>,
    /// Case-insensitive match. Defaults to `false`.
    #[serde(default)]
    case_insensitive: Option<bool>,
    /// Multi-line mode: lets `.` cross newlines and the pattern span lines.
    /// Only meaningful with `output_mode = "content"`.
    #[serde(default)]
    multiline: Option<bool>,
    /// Lines of surrounding context per match. Only meaningful with
    /// `output_mode = "content"`.
    #[serde(default)]
    context: Option<usize>,
    /// Cap on result lines kept from `rg`'s stdout. Defaults to
    /// [`DEFAULT_HEAD_LIMIT`], clamped to [`MAX_HEAD_LIMIT`].
    #[serde(default)]
    head_limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    /// rg `--files-with-matches` — one path per line. Default.
    #[default]
    FilesWithMatches,
    /// rg default — `path:line:text`. Includes line numbers.
    Content,
    /// rg `--count-matches` — `path:N` per file.
    Count,
}

/// Search file contents via the system `rg` binary.
pub struct Grep;

#[async_trait]
impl BaseTool for Grep {
    fn schema(&self) -> Tool {
        Tool {
            name: "grep".into(),
            description: "Search file contents via ripgrep (`rg`). Respects \
                .gitignore and skips binary/hidden files by default. \
                output_mode: files_with_matches (default, paths only) | \
                content (path:line:text with line numbers, supports `context` \
                and `multiline`) | count (path:N per file). Pattern uses Rust \
                regex syntax. Path defaults to the workspace root but may \
                point anywhere readable on disk. Requires the `rg` binary \
                on PATH."
                .into(),
            parameters: serde_json::to_value(schema_for!(GrepParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome> {
        let p: GrepParams =
            serde_json::from_value(args).map_err(|source| Error::InvalidArguments {
                tool: "grep".into(),
                source,
            })?;

        if p.pattern.is_empty() {
            return Err(exec("pattern must not be empty"));
        }
        let mode = p.output_mode.unwrap_or_default();
        let head_limit = p
            .head_limit
            .unwrap_or(DEFAULT_HEAD_LIMIT)
            .clamp(1, MAX_HEAD_LIMIT);

        let root = workspace::root();
        let search_path = workspace::resolve_for_read(p.path.as_deref())
            .await
            .map_err(|e| exec(e.to_string()))?;

        let rg_args = build_argv(&p, mode, &search_path);

        let mut cmd = Command::new("rg");
        cmd.args(&rg_args);
        cmd.current_dir(&root);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                exec(
                    "ripgrep (rg) not found on PATH; install via `brew install ripgrep`, \
                     `apt install ripgrep`, or `cargo install ripgrep`",
                )
            } else {
                exec(format!("spawn rg: {e}"))
            }
        })?;

        let output = match timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(exec(format!("wait failed: {e}"))),
            Err(_) => {
                return Err(exec(format!(
                    "rg timed out after {}s",
                    COMMAND_TIMEOUT.as_secs()
                )));
            }
        };

        match output.status.code() {
            Some(0) => {
                let body = format_stdout(&output.stdout, head_limit);
                Ok(Value::String(body).into())
            }
            Some(1) => Ok(Value::String("(no matches)".into()).into()),
            other => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let trimmed = stderr.trim();
                let suffix = if trimmed.is_empty() {
                    String::new()
                } else {
                    format!(": {trimmed}")
                };
                let code = other.map_or_else(|| "signal".to_string(), |c| c.to_string());
                Err(exec(format!("rg exited with code {code}{suffix}")))
            }
        }
    }
}

/// Build the rg argv vector from the parsed parameters.
///
/// Kept as a free function so unit tests can assert the exact argv shape
/// without spawning a subprocess.
fn build_argv(p: &GrepParams, mode: OutputMode, search_path: &Path) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(16);
    argv.push("--color=never".into());
    argv.push("--no-messages".into());

    match mode {
        OutputMode::FilesWithMatches => {
            argv.push("--files-with-matches".into());
            argv.push("--sortr=modified".into());
        }
        OutputMode::Content => {
            argv.push("--line-number".into());
            argv.push("--with-filename".into());
        }
        OutputMode::Count => {
            argv.push("--count-matches".into());
        }
    }

    if p.case_insensitive.unwrap_or(false) {
        argv.push("--ignore-case".into());
    }

    if mode == OutputMode::Content {
        if p.multiline.unwrap_or(false) {
            argv.push("--multiline".into());
            argv.push("--multiline-dotall".into());
        }
        if let Some(n) = p.context {
            argv.push("--context".into());
            argv.push(n.to_string());
        }
    }

    if let Some(g) = p.glob.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        argv.push("--glob".into());
        argv.push(g.into());
    }

    argv.push("--regexp".into());
    argv.push(p.pattern.clone());
    argv.push("--".into());
    argv.push(search_path.to_string_lossy().into_owned());
    argv
}

/// Trim `rg`'s stdout to at most `head_limit` lines and the shared byte cap.
fn format_stdout(stdout: &[u8], head_limit: usize) -> String {
    let text = String::from_utf8_lossy(stdout);
    let mut out = String::new();
    let mut kept = 0usize;
    let mut total = 0usize;
    let mut byte_truncated = false;
    let mut line_truncated = false;

    for line in text.split_inclusive('\n') {
        total += 1;
        if kept >= head_limit {
            line_truncated = true;
            continue;
        }
        if out.len() + line.len() > MAX_TOOL_RESULT_BYTES {
            byte_truncated = true;
            break;
        }
        out.push_str(line);
        kept += 1;
    }

    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }

    if byte_truncated {
        let _ = writeln!(
            out,
            "(truncated at ~{}KB; lower head_limit or narrow the search)",
            MAX_TOOL_RESULT_BYTES / 1000
        );
    } else if line_truncated {
        let _ = writeln!(
            out,
            "(showing {kept} of {total} lines; raise head_limit to see more)"
        );
    }
    out
}

/// Resolve the search path to an absolute path inside the workspace.
fn exec(message: impl Into<String>) -> Error {
    Error::Execution {
        tool: "grep".into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rg_available() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn argv_files_with_matches_default() {
        let p = GrepParams {
            pattern: "TODO".into(),
            path: None,
            glob: None,
            output_mode: None,
            case_insensitive: None,
            multiline: None,
            context: None,
            head_limit: None,
        };
        let argv = build_argv(&p, OutputMode::FilesWithMatches, Path::new("/tmp/x"));
        assert!(argv.iter().any(|a| a == "--files-with-matches"));
        assert!(argv.iter().any(|a| a == "--sortr=modified"));
        assert!(argv.iter().any(|a| a == "--regexp"));
        assert!(argv.iter().any(|a| a == "TODO"));
    }

    #[test]
    fn argv_content_mode_includes_line_numbers() {
        let p = GrepParams {
            pattern: "fn ".into(),
            path: None,
            glob: Some("*.rs".into()),
            output_mode: Some(OutputMode::Content),
            case_insensitive: Some(true),
            multiline: None,
            context: Some(2),
            head_limit: None,
        };
        let argv = build_argv(&p, OutputMode::Content, Path::new("/tmp/x"));
        assert!(argv.iter().any(|a| a == "--line-number"));
        assert!(argv.iter().any(|a| a == "--with-filename"));
        assert!(argv.iter().any(|a| a == "--ignore-case"));
        assert!(argv.iter().any(|a| a == "--context"));
        assert!(argv.iter().any(|a| a == "2"));
        assert!(argv.iter().any(|a| a == "--glob"));
        assert!(argv.iter().any(|a| a == "*.rs"));
    }

    #[test]
    fn argv_multiline_only_in_content_mode() {
        let p = GrepParams {
            pattern: "x".into(),
            path: None,
            glob: None,
            output_mode: Some(OutputMode::FilesWithMatches),
            case_insensitive: None,
            multiline: Some(true),
            context: Some(5),
            head_limit: None,
        };
        let argv = build_argv(&p, OutputMode::FilesWithMatches, Path::new("/tmp/x"));
        assert!(!argv.iter().any(|a| a == "--multiline"));
        assert!(!argv.iter().any(|a| a == "--context"));
    }

    #[test]
    fn format_stdout_caps_lines() {
        let stdout = b"a.rs\nb.rs\nc.rs\nd.rs\n";
        let body = format_stdout(stdout, 2);
        assert!(body.starts_with("a.rs\nb.rs\n"));
        assert!(body.contains("showing 2 of 4 lines"));
    }

    #[test]
    fn format_stdout_no_truncation_marker_when_under_limit() {
        let body = format_stdout(b"x\ny\n", 10);
        assert_eq!(body, "x\ny\n");
    }

    #[tokio::test]
    async fn empty_pattern_rejected() {
        let err = Grep
            .call(json!({ "pattern": "" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("pattern must not be empty"));
    }

    #[tokio::test]
    async fn nonexistent_path_returns_clean_error() {
        let err = Grep
            .call(json!({
                "pattern": "x",
                "path": "/this/path/does/not/exist/abc-xyz-123",
            }))
            .await
            .unwrap_err()
            .to_string();
        // Path resolution surfaces the canonicalize error, not a
        // workspace-containment error (read access is unbounded).
        assert!(!err.contains("outside workspace"));
    }

    #[tokio::test]
    async fn no_matches_returns_sentinel() {
        if !rg_available() {
            return;
        }
        // Cargo.toml is small and stable; a UUID-shaped pattern has zero
        // chance of appearing in it.
        let out = Grep
            .call(json!({
                "pattern": "ZZQ-9F8E7D6C-NO-MATCH-XYZ",
                "path": "Cargo.toml",
                "output_mode": "content",
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(Value::String(body)) = out else {
            panic!("expected string result");
        };
        assert!(body.contains("(no matches)"), "body was: {body}");
    }

    #[tokio::test]
    async fn finds_real_match_in_self() {
        if !rg_available() {
            return;
        }
        let out = Grep
            .call(json!({
                "pattern": "DEFAULT_HEAD_LIMIT",
                "path": "src/tools/grep.rs",
                "output_mode": "files_with_matches",
            }))
            .await
            .unwrap();
        let ToolOutcome::Result(Value::String(body)) = out else {
            panic!("expected string result");
        };
        assert!(body.contains("grep.rs"), "body was: {body}");
    }
}
