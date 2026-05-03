//! Interactive first-run bootstrap for `~/.mandeven/mandeven.toml`.
//!
//! When the per-user config file is missing and stdin is attached to
//! a terminal, [`interactive`] walks the user through a minimal set of
//! prompts (provider selection, upstream model name, profile alias,
//! context window, plus three optional sampling / transport knobs) and
//! returns a fresh [`AppConfig`]. The caller is responsible for
//! stamping `source_path` and persisting to disk — this module is
//! pure construction + I/O against the terminal, not filesystem
//! state.
//!
//! Model-specific defaults are deliberately not hard-coded here:
//! every value the user could reasonably disagree with is elicited
//! from the prompt instead of baked into the bootstrap logic.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::str::FromStr;

use super::error::{ConfigError, Result};
use super::types::{
    AgentConfig, AppConfig, ChannelsConfig, DiscordConfig, LLMConfig, LLMProfile, TuiConfig,
    WechatConfig,
};
use crate::llm::providers;

/// Drive the interactive bootstrap flow and return the user-constructed
/// config.
///
/// `source_path` is left empty on the returned value; the caller (see
/// [`super::loader`]) stamps it before calling [`AppConfig::save`].
///
/// # Errors
///
/// - [`ConfigError::NotInteractive`] when stdin is not a tty.
/// - [`ConfigError::Aborted`] when the user closes stdin (Ctrl-D) at
///   any prompt.
/// - [`ConfigError::Io`] on any other stdin / stdout failure.
pub(super) fn interactive() -> Result<AppConfig> {
    if !io::stdin().is_terminal() {
        return Err(ConfigError::NotInteractive);
    }

    let mut stdout = io::stdout().lock();
    let stdin = io::stdin();
    let mut stdin = stdin.lock();

    writeln!(
        stdout,
        "No mandeven.toml found at {} — let's create one.",
        super::paths::config_path().display()
    )?;
    writeln!(stdout)?;

    let provider = prompt_provider(&mut stdin, &mut stdout)?;
    let model_name = prompt_required(&mut stdin, &mut stdout, "Upstream model name")?;
    let profile_alias = prompt_default_str(&mut stdin, &mut stdout, "Profile alias", &model_name)?;
    let max_context_window =
        prompt_parsed_required::<u32>(&mut stdin, &mut stdout, "Max context window (tokens)")?;

    writeln!(stdout)?;
    writeln!(stdout, "Optional — press Enter to skip:")?;
    let max_tokens = prompt_parsed_optional::<u32>(&mut stdin, &mut stdout, "  Max tokens")?;
    let temperature = prompt_parsed_optional::<f32>(&mut stdin, &mut stdout, "  Temperature")?;
    let timeout_secs =
        prompt_parsed_optional::<u64>(&mut stdin, &mut stdout, "  HTTP timeout (seconds)")?;

    let profile = LLMProfile {
        model_name,
        max_context_window,
        max_tokens,
        temperature,
        // Bootstrap doesn't prompt for `thinking`: it's a niche
        // knob that only matters on a subset of providers. Users
        // who want it edit `mandeven.toml` after first run.
        thinking: None,
    };
    let mut models = HashMap::new();
    models.insert(profile_alias.clone(), profile);
    let mut providers_map = HashMap::new();
    providers_map.insert(provider.clone(), models);

    Ok(AppConfig {
        llm: LLMConfig {
            default: format!("{provider}/{profile_alias}"),
            timeout_secs,
            providers: providers_map,
        },
        tui: TuiConfig::default(),
        agent: AgentConfig::default(),
        sandbox: crate::security::SandboxConfig::default(),
        // Seed `[channels.discord]` even though the bot is off by
        // default. The section's mere presence opts the user into the
        // adapter, so `/discord` works on day one without the
        // user having to hand-edit `mandeven.toml`.
        channels: ChannelsConfig {
            discord: Some(DiscordConfig {
                enabled: false,
                token_env: "DISCORD_BOT_TOKEN".to_string(),
            }),
            wechat: Some(WechatConfig {
                enabled: false,
                token_env: "WECHAT_TOKEN".to_string(),
                account_id_env: "WECHAT_ACCOUNT_ID".to_string(),
                base_url: "https://ilinkai.weixin.qq.com".to_string(),
                login_timeout_secs: 480,
            }),
        },
        source_path: PathBuf::new(),
    })
}

/// Auto-select the sole registered provider when there is only one;
/// otherwise list the registered names and loop until the user picks
/// one that exists in [`providers::REGISTERED`].
fn prompt_provider(stdin: &mut impl BufRead, stdout: &mut impl Write) -> Result<String> {
    let registered = providers::REGISTERED;
    if let [single] = registered {
        writeln!(
            stdout,
            "Provider: {single} (only provider registered; auto-selected)"
        )?;
        return Ok((*single).to_string());
    }

    writeln!(stdout, "Registered providers: {}", registered.join(", "))?;
    loop {
        write!(stdout, "Provider: ")?;
        stdout.flush()?;
        let line = read_line(stdin)?;
        let trimmed = line.trim();
        if registered.contains(&trimmed) {
            return Ok(trimmed.to_string());
        }
        writeln!(
            stdout,
            "(not in the registered list; type one of: {})",
            registered.join(", ")
        )?;
    }
}

/// Prompt for a non-empty string, re-asking on empty input. EOF at the
/// prompt is propagated as [`ConfigError::Aborted`].
fn prompt_required(
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    label: &str,
) -> Result<String> {
    loop {
        write!(stdout, "{label}: ")?;
        stdout.flush()?;
        let line = read_line(stdin)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            writeln!(stdout, "(required; please enter a value)")?;
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

/// Prompt for a string with a default. Empty line accepts the default;
/// any non-empty input is trimmed and returned.
fn prompt_default_str(
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    write!(stdout, "{label} [{default}]: ")?;
    stdout.flush()?;
    let line = read_line(stdin)?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

/// Prompt for a required parseable value. Empty input or parse failure
/// shows a reminder and loops.
fn prompt_parsed_required<T>(
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    label: &str,
) -> Result<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    loop {
        write!(stdout, "{label}: ")?;
        stdout.flush()?;
        let line = read_line(stdin)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            writeln!(stdout, "(required; please enter a value)")?;
            continue;
        }
        match trimmed.parse::<T>() {
            Ok(value) => return Ok(value),
            Err(err) => writeln!(stdout, "(invalid value: {err}; try again)")?,
        }
    }
}

/// Prompt for an optional parseable value. Empty line returns `None`;
/// parse failure loops.
fn prompt_parsed_optional<T>(
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    label: &str,
) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    loop {
        write!(stdout, "{label} [skip]: ")?;
        stdout.flush()?;
        let line = read_line(stdin)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        match trimmed.parse::<T>() {
            Ok(value) => return Ok(Some(value)),
            Err(err) => writeln!(stdout, "(invalid value: {err}; try again or Enter to skip)")?,
        }
    }
}

/// Read one line from `stdin`. Treats EOF (zero-byte read) as
/// [`ConfigError::Aborted`] so Ctrl-D at any prompt cleanly cancels
/// bootstrap without partial state.
fn read_line(stdin: &mut impl BufRead) -> Result<String> {
    let mut buf = String::new();
    let n = stdin.read_line(&mut buf)?;
    if n == 0 {
        return Err(ConfigError::Aborted);
    }
    Ok(buf)
}
