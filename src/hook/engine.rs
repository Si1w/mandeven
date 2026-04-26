//! [`HookEngine`] — loads `hooks.json` once at boot, then fires
//! matching hooks at lifecycle events.
//!
//! Thread model: each `fire()` call runs every matching hook
//! sequentially within the call. Hooks of the same event from
//! different matchers all run; their outcomes aggregate into one
//! [`HookFireResult`]. Concurrent firing is intentionally not
//! supported v1 — reasoning: hooks may have ordering side-effects on
//! shared resources (audit logs, lock files), and 9 events × few
//! hooks is rarely hot enough to need parallelism.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use super::error::{Error, Result};
use super::types::{CommandHook, HookEvent, HookFile, HookMatcher};
use super::{HOOK_TIMEOUT_SECS_DEFAULT, HOOKS_FILENAME};

/// Outcome of one hook command execution.
#[derive(Debug, Clone)]
pub struct HookOutcome {
    /// The exact `command` string from the hook config.
    pub command: String,
    /// `sh -c` exit status (`-1` if the process couldn't be spawned
    /// or was killed by signal).
    pub exit_code: i32,
    /// Captured stdout. Logged but not surfaced to the model.
    pub stdout: String,
    /// Captured stderr. Same handling as stdout.
    pub stderr: String,
    /// `true` when this hook chose to block the surrounding event
    /// (either via `{"decision": "block"}` JSON on stdout, or via
    /// non-zero exit + `block_on_nonzero_exit`).
    pub blocked: bool,
    /// Human-readable explanation when `blocked` is true. `None`
    /// otherwise.
    pub reason: Option<String>,
}

/// Aggregate of every hook fired for one event.
#[derive(Debug, Clone, Default)]
pub struct HookFireResult {
    /// Per-hook outcomes in the order they ran.
    pub outcomes: Vec<HookOutcome>,
}

impl HookFireResult {
    /// `true` when at least one hook chose to block.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.outcomes.iter().any(|o| o.blocked)
    }

    /// First blocking outcome's reason. `None` if nothing blocked or
    /// if the blocking outcome carried no reason.
    #[must_use]
    pub fn block_reason(&self) -> Option<&str> {
        self.outcomes
            .iter()
            .find(|o| o.blocked)
            .and_then(|o| o.reason.as_deref())
    }
}

/// Hook engine. Loaded once at boot, shared via `Arc<HookEngine>`.
#[derive(Debug)]
pub struct HookEngine {
    enabled: bool,
    file: HookFile,
    /// Pre-compiled regexes keyed by `(event, matcher_index)`. Built
    /// at [`Self::load`] so per-fire cost is `O(matched matchers)`.
    /// Matchers without a `matcher` field are absent here and
    /// "always match" downstream.
    matchers: HashMap<(HookEvent, usize), Regex>,
    /// Resolved `~/.mandeven/` (or `$MANDEVEN_HOME`). Surfaces as
    /// `MANDEVEN_PROJECT_DIR` env var to every hook.
    data_dir: PathBuf,
}

impl HookEngine {
    /// Construct an engine.
    ///
    /// `enabled = false` ⇒ no file read; the engine answers every
    /// `fire()` with an empty result.
    /// Missing `hooks.json` ⇒ same outcome as an empty file
    /// (no matchers, every fire is a no-op).
    ///
    /// # Errors
    ///
    /// - [`Error::FileRead`] when `hooks.json` exists but the read
    ///   fails.
    /// - [`Error::Parse`] when the file contents do not deserialize.
    /// - [`Error::InvalidMatcher`] when any matcher field is not a
    ///   valid regex.
    pub fn load(enabled: bool, data_dir: &Path) -> Result<Self> {
        if !enabled {
            return Ok(Self {
                enabled: false,
                file: HookFile::default(),
                matchers: HashMap::new(),
                data_dir: data_dir.to_path_buf(),
            });
        }

        let path = data_dir.join(HOOKS_FILENAME);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    enabled: true,
                    file: HookFile::default(),
                    matchers: HashMap::new(),
                    data_dir: data_dir.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(Error::FileRead { path, source });
            }
        };

        let file: HookFile = serde_json::from_str(&raw).map_err(|source| Error::Parse {
            path: path.clone(),
            source,
        })?;

        let matchers = compile_matchers(&file)?;

        Ok(Self {
            enabled: true,
            file,
            matchers,
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// Fire every matching hook for `event` and aggregate the
    /// outcomes.
    ///
    /// `target` is the matcher key — tool name for `Pre/PostToolUse`,
    /// job name for `CronTick`, `None` for events without a target
    /// dimension.
    ///
    /// `payload` is the event-specific JSON object passed on stdin.
    /// Caller assembles it; this engine passes through verbatim plus
    /// the standard `hook_event_name`, `session_id`, `cwd` envelope.
    pub async fn fire(
        &self,
        event: HookEvent,
        target: Option<&str>,
        mut payload: Value,
        session_id: &str,
        cwd: &Path,
    ) -> HookFireResult {
        if !self.enabled || self.file.is_empty() {
            return HookFireResult::default();
        }

        // Inject envelope fields. If caller's payload already has
        // them, preserve caller values (event-specific code knows
        // best).
        if let Value::Object(map) = &mut payload {
            map.entry("hook_event_name".to_string())
                .or_insert_with(|| Value::String(format!("{event:?}")));
            map.entry("session_id".to_string())
                .or_insert_with(|| Value::String(session_id.to_string()));
            map.entry("cwd".to_string())
                .or_insert_with(|| Value::String(cwd.display().to_string()));
        }

        let stdin_bytes = match serde_json::to_string(&payload) {
            Ok(s) => format!("{s}\n").into_bytes(),
            Err(err) => {
                eprintln!("[hook] failed to serialize {event:?} payload: {err}");
                return HookFireResult::default();
            }
        };

        let mut outcomes = Vec::new();
        for (idx, matcher_block) in self.file.matchers(event).iter().enumerate() {
            if !self.matcher_matches(event, idx, matcher_block, target) {
                continue;
            }
            for hook in &matcher_block.hooks {
                let outcome = self
                    .run_one(event, hook, &stdin_bytes, session_id, cwd)
                    .await;
                outcomes.push(outcome);
            }
        }

        HookFireResult { outcomes }
    }

    /// Test whether `matcher_block` applies to the current target.
    /// `None` matcher = "match everything"; events without a target
    /// also "match everything" (matcher field is documentation only
    /// for those events).
    fn matcher_matches(
        &self,
        event: HookEvent,
        idx: usize,
        block: &HookMatcher,
        target: Option<&str>,
    ) -> bool {
        let Some(_) = block.matcher.as_ref() else {
            return true;
        };
        let Some(target) = target else {
            return true;
        };
        let Some(re) = self.matchers.get(&(event, idx)) else {
            return true;
        };
        re.is_match(target)
    }

    /// Spawn one hook, write the stdin payload, await the result
    /// (with timeout), and translate stdout / exit code into a
    /// [`HookOutcome`].
    async fn run_one(
        &self,
        event: HookEvent,
        hook: &CommandHook,
        stdin_bytes: &[u8],
        session_id: &str,
        cwd: &Path,
    ) -> HookOutcome {
        let timeout_secs = hook.timeout_secs.unwrap_or(HOOK_TIMEOUT_SECS_DEFAULT);
        let timeout_dur = Duration::from_secs(timeout_secs);

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&hook.command);
        cmd.env("MANDEVEN_PROJECT_DIR", &self.data_dir);
        cmd.env("MANDEVEN_SESSION_ID", session_id);
        cmd.env("MANDEVEN_CWD", cwd);
        cmd.env("MANDEVEN_HOOK_EVENT", format!("{event:?}"));
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                return HookOutcome {
                    command: hook.command.clone(),
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("spawn failed: {err}"),
                    blocked: hook.block_on_nonzero_exit,
                    reason: hook
                        .block_on_nonzero_exit
                        .then(|| format!("hook spawn failed: {err}")),
                };
            }
        };

        // Write stdin first, then await output. If stdin write
        // fails the hook is likely already gone — treat as a soft
        // error and still wait for whatever output it produced.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_bytes).await;
            // Explicit drop closes the pipe — bash `read -r` returns
            // EOF and the hook proceeds.
            drop(stdin);
        }

        let output = match timeout(timeout_dur, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(err)) => {
                return HookOutcome {
                    command: hook.command.clone(),
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("wait failed: {err}"),
                    blocked: hook.block_on_nonzero_exit,
                    reason: hook
                        .block_on_nonzero_exit
                        .then(|| format!("hook wait failed: {err}")),
                };
            }
            Err(_) => {
                return HookOutcome {
                    command: hook.command.clone(),
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("timed out after {timeout_secs}s"),
                    blocked: hook.block_on_nonzero_exit,
                    reason: hook
                        .block_on_nonzero_exit
                        .then(|| format!("hook timed out after {timeout_secs}s")),
                };
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);

        // Decision precedence:
        // 1. JSON `{"decision": "block", "reason": ...}` on stdout
        //    blocks regardless of exit code.
        // 2. Otherwise, non-zero exit + `block_on_nonzero_exit`
        //    blocks with stderr (or exit-code message) as reason.
        let json_decision = parse_decision(&stdout);
        let (blocked, reason) = match json_decision {
            Some(Decision::Block(reason)) => (true, Some(reason)),
            None if exit_code != 0 && hook.block_on_nonzero_exit => {
                let reason = if !stderr.trim().is_empty() {
                    stderr.trim().to_string()
                } else if !stdout.trim().is_empty() {
                    stdout.trim().to_string()
                } else {
                    format!("exit {exit_code}")
                };
                (true, Some(reason))
            }
            // Approve / no decision / non-blocking exit error all
            // resolve to "do not block".
            Some(Decision::Approve) | None => (false, None),
        };

        HookOutcome {
            command: hook.command.clone(),
            exit_code,
            stdout,
            stderr,
            blocked,
            reason,
        }
    }
}

/// Parsed decision from a hook's stdout. Both fields here mirror
/// Claude Code's protocol; mandeven only acts on `Block`.
enum Decision {
    Block(String),
    Approve,
}

#[derive(Deserialize)]
struct DecisionPayload {
    decision: Option<String>,
    reason: Option<String>,
}

/// Try to parse hook stdout as a decision JSON. Returns `None` when
/// the output isn't valid JSON or doesn't carry a `decision` field
/// — those cases fall back to exit-code-based blocking.
fn parse_decision(stdout: &str) -> Option<Decision> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let payload: DecisionPayload = serde_json::from_str(trimmed).ok()?;
    match payload.decision.as_deref() {
        Some("block") => Some(Decision::Block(
            payload.reason.unwrap_or_else(|| "blocked by hook".into()),
        )),
        Some("approve") => Some(Decision::Approve),
        _ => None,
    }
}

/// Pre-compile every matcher regex. Indexed by
/// `(event, matcher_block_index)` so the runtime lookup is `O(1)`.
fn compile_matchers(file: &HookFile) -> Result<HashMap<(HookEvent, usize), Regex>> {
    let mut out = HashMap::new();
    for (event, matchers) in &file.events {
        for (idx, block) in matchers.iter().enumerate() {
            if let Some(pattern) = &block.matcher {
                let re = Regex::new(pattern).map_err(|source| Error::InvalidMatcher {
                    pattern: pattern.clone(),
                    source,
                })?;
                out.insert((*event, idx), re);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("mandeven-hook-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_hooks(dir: &Path, raw: &str) {
        std::fs::write(dir.join(HOOKS_FILENAME), raw).unwrap();
    }

    #[tokio::test]
    async fn disabled_engine_fires_nothing() {
        let dir = tempdir();
        write_hooks(
            &dir,
            r#"{"UserPromptSubmit":[{"hooks":[{"command":"true"}]}]}"#,
        );
        let engine = HookEngine::load(false, &dir).unwrap();
        let result = engine
            .fire(HookEvent::UserPromptSubmit, None, json!({}), "sess", &dir)
            .await;
        assert!(result.outcomes.is_empty());
    }

    #[tokio::test]
    async fn missing_file_yields_empty_engine() {
        let dir = tempdir();
        let engine = HookEngine::load(true, &dir).unwrap();
        let result = engine
            .fire(HookEvent::Stop, None, json!({}), "sess", &dir)
            .await;
        assert!(result.outcomes.is_empty());
    }

    #[tokio::test]
    async fn matcher_filters_by_target() {
        let dir = tempdir();
        write_hooks(
            &dir,
            r#"{"PreToolUse":[
                {"matcher":"shell","hooks":[{"command":"true"}]},
                {"matcher":"file_.*","hooks":[{"command":"true"}]}
            ]}"#,
        );
        let engine = HookEngine::load(true, &dir).unwrap();

        let r = engine
            .fire(
                HookEvent::PreToolUse,
                Some("shell"),
                json!({}),
                "sess",
                &dir,
            )
            .await;
        assert_eq!(r.outcomes.len(), 1);

        let r = engine
            .fire(
                HookEvent::PreToolUse,
                Some("file_write"),
                json!({}),
                "sess",
                &dir,
            )
            .await;
        assert_eq!(r.outcomes.len(), 1);

        let r = engine
            .fire(
                HookEvent::PreToolUse,
                Some("nothing-matches"),
                json!({}),
                "sess",
                &dir,
            )
            .await;
        assert!(r.outcomes.is_empty());
    }

    #[tokio::test]
    async fn nonzero_exit_with_block_flag_blocks() {
        let dir = tempdir();
        write_hooks(
            &dir,
            r#"{"UserPromptSubmit":[{"hooks":[
                {"command":"echo nope >&2; exit 1","block_on_nonzero_exit":true}
            ]}]}"#,
        );
        let engine = HookEngine::load(true, &dir).unwrap();
        let r = engine
            .fire(HookEvent::UserPromptSubmit, None, json!({}), "s", &dir)
            .await;
        assert!(r.is_blocked());
        assert_eq!(r.block_reason(), Some("nope"));
    }

    #[tokio::test]
    async fn nonzero_exit_without_block_flag_does_not_block() {
        let dir = tempdir();
        write_hooks(
            &dir,
            r#"{"UserPromptSubmit":[{"hooks":[{"command":"exit 1"}]}]}"#,
        );
        let engine = HookEngine::load(true, &dir).unwrap();
        let r = engine
            .fire(HookEvent::UserPromptSubmit, None, json!({}), "s", &dir)
            .await;
        assert!(!r.is_blocked());
        assert_eq!(r.outcomes[0].exit_code, 1);
    }

    #[tokio::test]
    async fn json_decision_block_overrides_exit_code() {
        let dir = tempdir();
        write_hooks(
            &dir,
            r#"{"UserPromptSubmit":[{"hooks":[
                {"command":"echo '{\"decision\":\"block\",\"reason\":\"audit failed\"}'"}
            ]}]}"#,
        );
        let engine = HookEngine::load(true, &dir).unwrap();
        let r = engine
            .fire(HookEvent::UserPromptSubmit, None, json!({}), "s", &dir)
            .await;
        assert!(r.is_blocked());
        assert_eq!(r.block_reason(), Some("audit failed"));
        assert_eq!(r.outcomes[0].exit_code, 0);
    }

    #[tokio::test]
    async fn timeout_kills_hung_hook_and_blocks_when_configured() {
        let dir = tempdir();
        write_hooks(
            &dir,
            r#"{"UserPromptSubmit":[{"hooks":[
                {"command":"sleep 5","timeout_secs":1,"block_on_nonzero_exit":true}
            ]}]}"#,
        );
        let engine = HookEngine::load(true, &dir).unwrap();
        let r = engine
            .fire(HookEvent::UserPromptSubmit, None, json!({}), "s", &dir)
            .await;
        assert!(r.is_blocked());
        assert!(r.block_reason().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn payload_arrives_on_stdin() {
        let dir = tempdir();
        let out_file = dir.join("captured.txt");
        let cmd = format!("cat > {}", out_file.display());
        let raw = serde_json::to_string(&json!({
            "UserPromptSubmit": [{
                "hooks": [{"command": cmd}]
            }]
        }))
        .unwrap();
        write_hooks(&dir, &raw);

        let engine = HookEngine::load(true, &dir).unwrap();
        engine
            .fire(
                HookEvent::UserPromptSubmit,
                None,
                json!({"prompt": "hello world"}),
                "session-xyz",
                &dir,
            )
            .await;

        let captured = std::fs::read_to_string(&out_file).unwrap();
        assert!(captured.contains("\"prompt\":\"hello world\""));
        assert!(captured.contains("\"hook_event_name\":\"UserPromptSubmit\""));
        assert!(captured.contains("\"session_id\":\"session-xyz\""));
    }

    #[tokio::test]
    async fn env_vars_set_for_hook() {
        let dir = tempdir();
        let out_file = dir.join("env.txt");
        let cmd = format!(
            "echo \"$MANDEVEN_HOOK_EVENT $MANDEVEN_SESSION_ID\" > {}",
            out_file.display()
        );
        let raw = serde_json::to_string(&json!({
            "Stop": [{"hooks": [{"command": cmd}]}]
        }))
        .unwrap();
        write_hooks(&dir, &raw);

        let engine = HookEngine::load(true, &dir).unwrap();
        engine
            .fire(HookEvent::Stop, None, json!({}), "session-abc", &dir)
            .await;

        let captured = std::fs::read_to_string(&out_file).unwrap();
        assert!(captured.contains("Stop session-abc"));
    }

    #[test]
    fn invalid_matcher_regex_surfaces_at_load() {
        let dir = tempdir();
        write_hooks(
            &dir,
            r#"{"PreToolUse":[{"matcher":"[unclosed","hooks":[{"command":"x"}]}]}"#,
        );
        let err = HookEngine::load(true, &dir).unwrap_err();
        assert!(matches!(err, Error::InvalidMatcher { .. }));
    }
}
