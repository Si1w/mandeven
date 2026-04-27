//! Allow-list for shell commands under [`super::SandboxPolicy::ReadOnly`].
//!
//! Tier-1 of the safety story (the [`crate::tools::shell`] deny patterns
//! are tier-0, always on). When the policy is `ReadOnly`,
//! [`crate::tools::shell::Shell::call`] routes the model's command
//! through [`ensure_safe_command`] before anything is spawned. The
//! function rejects:
//!
//! 1. Compound shell syntax — pipes, redirects, command substitution,
//!    background jobs, etc. `ReadOnly` mode is for "look at the project,
//!    don't touch it"; chaining commands defeats the static analysis
//!    that this allow-list relies on.
//! 2. Any command whose first token is not on [`ALWAYS_SAFE`].
//! 3. `find` invocations using mutating flags
//!    ([`UNSAFE_FIND`]).
//! 4. `rg` invocations using flags that shell out to external programs
//!    ([`UNSAFE_RG_NO_ARG`], [`UNSAFE_RG_WITH_ARG`]) — taken straight
//!    from codex's `is_known_safe_command`.
//! 5. `git` invocations whose subcommand is not on [`SAFE_GIT_SUBCMDS`].
//!
//! **Tokenisation is intentionally naive**: we split on whitespace, no
//! shell quoting. A command like `echo "hello world"` works because the
//! first token is still `echo`. A pathological case like `'cat' foo` is
//! rejected (first token `'cat'` isn't on the list), which is a
//! conservative false-negative we accept rather than embedding a full
//! shell parser.

use crate::tools::error::{Error, Result};

/// Characters that introduce shell control flow or external side effects.
/// Any one of them present in the command string flips it to "compound"
/// and gets it rejected outright under read-only.
const COMPOUND_CHARS: &[char] = &['|', '&', ';', '`', '$', '>', '<'];

/// First-token allow-list. Restricted to commands that, when invoked
/// without compound syntax, *cannot* write to disk, execute another
/// program, or open a network socket.
const ALWAYS_SAFE: &[&str] = &[
    // file/dir inspection
    "cat", "ls", "pwd", "stat", "file", "du", "df", "realpath", "basename", "dirname",
    // text inspection / transformation (read-only when no redirection)
    "head", "tail", "wc", "sort", "uniq", "cut", "tr", "rev", "nl", "grep",
    // identity / metadata
    "which", "whoami", "id", "uname", "date", "env", "printf", "echo", "true", "false",
];

/// `find` flags that delete files, write files, or execute external
/// commands. Source: codex `is_known_safe_command` (matches behaviour 1:1).
const UNSAFE_FIND: &[&str] = &[
    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0", "-fprintf",
];

/// `rg` flags that don't take an argument but invoke external tools.
const UNSAFE_RG_NO_ARG: &[&str] = &["--search-zip", "-z"];

/// `rg` flags that take an argument naming an external program.
const UNSAFE_RG_WITH_ARG: &[&str] = &["--pre", "--hostname-bin"];

/// Subcommands that, when used without [`UNSAFE_GIT_SUBCMD_FLAG`], cannot
/// mutate the working tree, the index, or remote refs.
const SAFE_GIT_SUBCMDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "branch",
    "rev-parse",
    "remote",
    "config",
    "ls-files",
    "ls-tree",
    "describe",
];

/// `git` subcommand-level flags that can write files (`--output=...`)
/// or run external tools (`--ext-diff`).
const UNSAFE_GIT_SUBCMD_FLAG: &[&str] = &["--output", "--ext-diff", "--textconv", "--exec"];

/// Reject `command` if any of the layered checks fire. Returns `Ok(())`
/// when the command is judged safe to run under read-only.
///
/// # Errors
///
/// [`Error::Execution`] with a message naming the specific reason
/// (compound syntax, unknown command, unsafe sub-flag).
pub fn ensure_safe_command(command: &str) -> Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(blocked("empty command"));
    }
    if is_compound(trimmed) {
        return Err(blocked(
            "compound shell syntax (pipes, redirects, $(...), backticks, &&, ;) is \
             not allowed under read_only sandbox policy",
        ));
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let cmd0 = tokens[0];

    if cmd0 == "find" {
        return check_find(&tokens[1..]);
    }
    if cmd0 == "git" {
        return check_git(&tokens[1..]);
    }
    if cmd0 == "rg" {
        return check_rg(&tokens[1..]);
    }
    if !ALWAYS_SAFE.contains(&cmd0) {
        return Err(blocked(&format!(
            "command `{cmd0}` is not on the read_only allow-list"
        )));
    }
    Ok(())
}

/// Cheap check for shell control characters. Public so
/// [`crate::tools::shell`] can short-circuit similar logic if needed.
#[must_use]
pub fn is_compound(command: &str) -> bool {
    command.chars().any(|c| COMPOUND_CHARS.contains(&c))
}

fn check_find(args: &[&str]) -> Result<()> {
    if let Some(bad) = args.iter().find(|a| UNSAFE_FIND.contains(*a)) {
        return Err(blocked(&format!(
            "`find {bad}` can mutate the filesystem or execute external commands"
        )));
    }
    Ok(())
}

fn check_rg(args: &[&str]) -> Result<()> {
    for arg in args {
        if UNSAFE_RG_NO_ARG.contains(arg) {
            return Err(blocked(&format!(
                "`rg {arg}` invokes external decompressors"
            )));
        }
        if UNSAFE_RG_WITH_ARG
            .iter()
            .any(|prefix| *arg == *prefix || arg.starts_with(&format!("{prefix}=")))
        {
            return Err(blocked(&format!(
                "`rg {arg}` can execute external commands"
            )));
        }
    }
    Ok(())
}

fn check_git(args: &[&str]) -> Result<()> {
    let Some(sub) = args.iter().find(|a| !a.starts_with('-')) else {
        return Err(blocked("`git` invocation must include a subcommand"));
    };
    if !SAFE_GIT_SUBCMDS.contains(sub) {
        return Err(blocked(&format!(
            "`git {sub}` is not on the read_only allow-list (allowed: {})",
            SAFE_GIT_SUBCMDS.join(", ")
        )));
    }
    if let Some(bad) = args
        .iter()
        .find(|a| UNSAFE_GIT_SUBCMD_FLAG.iter().any(|p| *p == **a))
    {
        return Err(blocked(&format!(
            "`git {bad}` can write files or invoke external programs"
        )));
    }
    Ok(())
}

fn blocked(message: &str) -> Error {
    Error::Execution {
        tool: "shell".into(),
        message: format!("{message}; current sandbox policy is read_only"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_basic_inspection_commands() {
        ensure_safe_command("ls -la").unwrap();
        ensure_safe_command("cat README.md").unwrap();
        ensure_safe_command("pwd").unwrap();
        ensure_safe_command("wc -l foo.txt").unwrap();
        ensure_safe_command("grep TODO src").unwrap();
    }

    #[test]
    fn rejects_unknown_command() {
        let err = ensure_safe_command("npm install").unwrap_err().to_string();
        assert!(err.contains("npm"));
        assert!(err.contains("read_only"));
    }

    #[test]
    fn rejects_compound_pipe() {
        assert!(ensure_safe_command("ls | head").is_err());
    }

    #[test]
    fn rejects_compound_redirect() {
        assert!(ensure_safe_command("ls > out.txt").is_err());
    }

    #[test]
    fn rejects_command_substitution() {
        assert!(ensure_safe_command("echo $(whoami)").is_err());
        assert!(ensure_safe_command("echo `pwd`").is_err());
    }

    #[test]
    fn allows_safe_find() {
        ensure_safe_command("find . -name *.rs").unwrap();
    }

    #[test]
    fn rejects_find_exec() {
        assert!(ensure_safe_command("find . -name *.rs -exec rm {} ;").is_err());
        assert!(ensure_safe_command("find . -delete").is_err());
    }

    #[test]
    fn allows_safe_rg() {
        ensure_safe_command("rg --json TODO src").unwrap();
    }

    #[test]
    fn rejects_unsafe_rg_flags() {
        assert!(ensure_safe_command("rg --pre xz pattern").is_err());
        assert!(ensure_safe_command("rg --pre=xz pattern").is_err());
        assert!(ensure_safe_command("rg -z pattern").is_err());
        assert!(ensure_safe_command("rg --search-zip pattern").is_err());
    }

    #[test]
    fn allows_safe_git_subcommands() {
        ensure_safe_command("git status").unwrap();
        ensure_safe_command("git log --oneline").unwrap();
        ensure_safe_command("git diff HEAD~1").unwrap();
        ensure_safe_command("git rev-parse HEAD").unwrap();
    }

    #[test]
    fn rejects_mutating_git_subcommands() {
        assert!(ensure_safe_command("git commit -m foo").is_err());
        assert!(ensure_safe_command("git push origin main").is_err());
        assert!(ensure_safe_command("git checkout dev").is_err());
        assert!(ensure_safe_command("git reset --hard HEAD").is_err());
    }

    #[test]
    fn rejects_git_unsafe_subcmd_flag() {
        assert!(ensure_safe_command("git diff --ext-diff").is_err());
        assert!(ensure_safe_command("git log --output").is_err());
    }

    #[test]
    fn rejects_empty_command() {
        assert!(ensure_safe_command("").is_err());
        assert!(ensure_safe_command("   ").is_err());
    }
}
