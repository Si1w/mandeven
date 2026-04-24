//! `shell` tool — execute a shell command via `sh -c`.
//!
//! Design notes:
//!
//! - **Shell choice**: `sh -c`, not `bash -l -c`. POSIX-portable and
//!   deterministic; the user's login profile is intentionally *not*
//!   sourced. Callers that need specific environment variables must
//!   pass them through the curated [`PASS_ENV_KEYS`] list or wrap the
//!   command explicitly.
//! - **Curated env**: only a small set of variables (`HOME`, `PATH`,
//!   `LANG`, `TERM`, `USER`) is inherited from the parent process.
//!   All other variables — including any API keys in the agent's
//!   environment — are stripped before `sh -c` runs.
//! - **Deny patterns**: a minimal regex set blocks a handful of
//!   obviously destructive commands (`rm -rf`, `dd if=`, `shutdown`,
//!   fork bomb). Not a sandbox; a defense-in-depth backstop against
//!   the most catastrophic accidents.
//! - **Timeout via `kill_on_drop`**: the child is spawned with
//!   `kill_on_drop(true)`; when the surrounding `tokio::time::timeout`
//!   fires, dropping the future cancels `wait_with_output` which
//!   drops the [`tokio::process::Child`], which SIGKILLs the process.
//! - **Unix-only**: no Windows branch. Re-add when a concrete need
//!   arises.
//! - **Output cap**: combined stdout + stderr is capped at the shared
//!   [`super::MAX_TOOL_RESULT_BYTES`]; overflow is trimmed from the
//!   middle so the head (likely command summary) and tail (likely
//!   error message) both survive.

use std::fmt::Write;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

use super::error::{Error, Result};
use super::{BaseTool, MAX_TOOL_RESULT_BYTES};
use crate::llm::Tool;

/// Default per-call timeout when the caller omits `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Hard ceiling on `timeout_secs`. Clamped on the way in.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Environment variables forwarded from the parent process into
/// `sh -c`. Keep this list short; everything else (API keys, etc.) is
/// stripped.
const PASS_ENV_KEYS: &[&str] = &["HOME", "PATH", "LANG", "TERM", "USER"];

/// Patterns that cause [`Shell::call`] to refuse the command outright.
/// Matched against the lowercased command. Compiled once into
/// [`DENY_RE`] on first use.
const DENY_PATTERNS: &[&str] = &[
    r"\brm\s+-[rf]{1,2}\b",            // rm -r, rm -rf, rm -fr
    r"\bdd\s+if=",                     // dd
    r"\b(shutdown|reboot|poweroff)\b", // system power
    r":\(\)\s*\{.*\};\s*:",            // classic fork bomb
    r"\b(mkfs|diskpart)\b",            // disk operations
];

/// Compiled form of [`DENY_PATTERNS`]. Initialized on first access.
///
/// # Panics
///
/// Initialization panics if any entry in [`DENY_PATTERNS`] fails to
/// compile — a build-time invariant: the patterns are static literals
/// validated on every `cargo check`.
static DENY_RE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    DENY_PATTERNS
        .iter()
        .map(|p| Regex::new(p).expect("static deny patterns are valid regex"))
        .collect()
});

#[derive(Deserialize, JsonSchema)]
struct ShellParams {
    /// Shell command passed to `sh -c`.
    command: String,
    /// Working directory. Defaults to the process CWD.
    #[serde(default)]
    cwd: Option<String>,
    /// Per-call timeout in seconds. Default 60, max 600.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Execute a command via `sh -c` with a curated environment, deny-list
/// guard, and bounded output.
///
/// Zero-sized: the regex deny list is a program-wide constant held in
/// [`DENY_RE`], so no per-instance state is needed.
pub struct Shell;

#[async_trait]
impl BaseTool for Shell {
    fn schema(&self) -> Tool {
        Tool {
            name: "shell".into(),
            description: "Execute a shell command via `sh -c` with a curated \
                environment (HOME, PATH, LANG, TERM, USER). Prefer file_read / \
                file_write / file_edit over cat / echo / sed, and grep tools over \
                shell find/grep. Output is middle-truncated at the shared tool \
                result cap; timeout defaults to 60s, max 600s. A minimal deny-list \
                blocks obviously destructive commands (rm -rf, dd if=, shutdown, \
                fork bomb)."
                .into(),
            parameters: serde_json::to_value(schema_for!(ShellParams))
                .expect("JsonSchema derive always serializes"),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let p: ShellParams =
            serde_json::from_value(args).map_err(|source| Error::InvalidArguments {
                tool: "shell".into(),
                source,
            })?;

        let lower = p.command.to_lowercase();
        if let Some(hit) = DENY_RE.iter().find(|re| re.is_match(&lower)) {
            return Err(exec(format!(
                "command blocked by deny pattern: {}",
                hit.as_str()
            )));
        }

        let t = Duration::from_secs(
            p.timeout_secs
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&p.command);
        cmd.env_clear();
        for key in PASS_ENV_KEYS {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        if let Some(dir) = &p.cwd {
            cmd.current_dir(dir);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let child = cmd
            .spawn()
            .map_err(|e| exec(format!("spawn failed: {e}")))?;

        let output = match timeout(t, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(exec(format!("wait failed: {e}"))),
            Err(_) => {
                return Err(exec(format!("timed out after {}s", t.as_secs())));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit = output.status.code().unwrap_or(-1);

        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.trim().is_empty() {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str("STDERR:\n");
            result.push_str(&stderr);
        }
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        write!(result, "Exit code: {exit}").expect("writing to String is infallible");

        Ok(Value::String(middle_truncate(
            &result,
            MAX_TOOL_RESULT_BYTES,
        )))
    }
}

fn exec(message: impl Into<String>) -> Error {
    Error::Execution {
        tool: "shell".into(),
        message: message.into(),
    }
}

/// If `text` exceeds `cap` bytes, keep the first and last `cap/2`
/// bytes with a `... N chars truncated ...` marker in between.
/// Split boundaries snap to the nearest UTF-8 char boundary so the
/// result stays valid.
fn middle_truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let half = cap / 2;
    let head_end = char_boundary_before(text, half);
    let tail_start = char_boundary_after(text, text.len() - half);
    let removed = text.len() - head_end - (text.len() - tail_start);
    format!(
        "{}\n\n... ({removed} bytes truncated) ...\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

fn char_boundary_before(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn char_boundary_after(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
