//! Clap-backed parser for slash-command bodies.
//!
//! Callers pass the text after the leading `/`. This parser owns command
//! shape and arity; each layer still decides whether a parsed command belongs
//! to it or should continue down the CLI → gateway → agent chain.

use clap::{ArgAction, Args, Parser, Subcommand};

/// Parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/help`
    Help,
    /// `/skills`
    Skills,
    /// `/exit`
    Exit,
    /// `/quit`
    Quit,
    /// `/new`
    New,
    /// `/list`
    List,
    /// `/load <n>`
    Load { index: usize },
    /// `/switch ...`
    Switch(SwitchCommand),
    /// `/compact [focus...]`
    Compact { focus: Option<String> },
    /// `/heartbeat ...`
    Heartbeat(HeartbeatCommand),
    /// `/cron ...`
    Cron(CronCommand),
    /// `/memory ...`
    Memory(MemoryCommand),
    /// `/discord ...`
    Discord(DiscordCommand),
    /// `/wechat ...`
    Wechat(WechatCommand),
    /// Unknown slash command. CLI uses this for skill lookup; the
    /// agent turns surviving externals into "unknown command".
    External { name: String, args: Vec<String> },
}

/// Parsed `/switch` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchCommand {
    /// `/switch`
    List,
    /// `/switch <provider/profile>`
    Runtime { profile_id: String },
    /// `/switch default`
    ShowDefault,
    /// `/switch default <provider/profile>`
    SetDefault { profile_id: String },
}

/// Parsed `/heartbeat` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatCommand {
    /// `/heartbeat`
    Status,
    /// `/heartbeat pause`
    Pause,
    /// `/heartbeat resume`
    Resume,
    /// `/heartbeat trigger`
    Trigger,
    /// `/heartbeat interval <seconds>`
    Interval { seconds: u64 },
}

/// Parsed `/cron` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronCommand {
    /// `/cron` or `/cron list`
    List,
    /// `/cron trigger <id>`
    Trigger { id: String },
    /// `/cron enable <id>`
    Enable { id: String },
    /// `/cron disable <id>`
    Disable { id: String },
    /// `/cron remove <id>`
    Remove { id: String },
}

/// Parsed `/memory` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCommand {
    /// `/memory` or `/memory list`
    List,
    /// `/memory search <query...>`
    Search { query: String },
    /// `/memory show <id>`
    Show { id: String },
    /// `/memory forget <id>`
    Forget { id: String },
    /// `/memory profile`
    Profile,
}

/// Parsed `/discord` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscordCommand {
    /// `/discord` (no args) — flip the gateway connection. Off → on,
    /// on → off. The current state is visible via [`Self::Status`].
    Toggle,
    /// `/discord status` — runtime snapshot.
    Status,
    /// `/discord list` — show the allow list.
    List,
    /// `/discord allow <user_id>`
    Allow { user_id: u64 },
    /// `/discord deny <user_id>`
    Deny { user_id: u64 },
    /// `/discord autostart on|off` — persist the boot-time
    /// `[channels.discord].enabled` flag in `mandeven.toml`.
    Autostart { on: bool },
}

/// Parsed `/wechat` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WechatCommand {
    /// `/wechat` (no args) — flip the iLink connection.
    Toggle,
    /// `/wechat status` — runtime snapshot.
    Status,
    /// `/wechat login` — scan QR code and save account credentials.
    Login,
    /// `/wechat logout` — stop and delete the saved active/latest account.
    Logout,
    /// `/wechat list` — show the allow list.
    List,
    /// `/wechat allow <user_id>`
    Allow { user_id: String },
    /// `/wechat deny <user_id>`
    Deny { user_id: String },
    /// `/wechat autostart on|off` — persist `[channels.wechat].enabled`.
    Autostart { on: bool },
}

/// Parse a slash-command body, excluding the leading `/`.
///
/// # Errors
///
/// Returns a concise usage or validation message when clap rejects the command
/// shape, for example missing required arguments or extra positional tokens.
pub fn parse(body: &str) -> Result<SlashCommand, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("usage: /<command>".to_string());
    }

    let raw = RawSlash::try_parse_from(body.split_whitespace())
        .map_err(|err| format_parse_error(&err))?;
    raw.command.try_into()
}

#[derive(Debug, Parser)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true
)]
struct RawSlash {
    #[command(subcommand)]
    command: RawCommand,
}

#[derive(Debug, Subcommand)]
enum RawCommand {
    Help,
    Skills,
    Exit,
    Quit,
    New,
    List,
    Load {
        #[arg(value_name = "n")]
        index: usize,
    },
    Switch(SwitchArgs),
    Compact(CompactArgs),
    Heartbeat(HeartbeatArgs),
    Cron(CronArgs),
    Memory(MemoryArgs),
    Discord(DiscordArgs),
    Wechat(WechatArgs),
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Debug, Args)]
struct CompactArgs {
    #[arg(
        value_name = "focus",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    focus: Vec<String>,
}

#[derive(Debug, Args)]
struct SwitchArgs {
    #[command(subcommand)]
    command: Option<SwitchSubcommand>,
}

#[derive(Debug, Subcommand)]
enum SwitchSubcommand {
    Default {
        #[arg(value_name = "provider/profile")]
        profile_id: Option<String>,
    },
    #[command(external_subcommand)]
    Runtime(Vec<String>),
}

#[derive(Debug, Args)]
struct HeartbeatArgs {
    #[command(subcommand)]
    command: Option<HeartbeatSubcommand>,
}

#[derive(Debug, Subcommand)]
enum HeartbeatSubcommand {
    Pause,
    Resume,
    Trigger,
    Interval {
        #[arg(value_name = "seconds")]
        seconds: u64,
    },
}

#[derive(Debug, Args)]
struct CronArgs {
    #[command(subcommand)]
    command: Option<CronSubcommand>,
}

#[derive(Debug, Subcommand)]
enum CronSubcommand {
    List,
    Trigger {
        #[arg(value_name = "id")]
        id: String,
    },
    Enable {
        #[arg(value_name = "id")]
        id: String,
    },
    Disable {
        #[arg(value_name = "id")]
        id: String,
    },
    Remove {
        #[arg(value_name = "id")]
        id: String,
    },
}

#[derive(Debug, Args)]
struct MemoryArgs {
    #[command(subcommand)]
    command: Option<MemorySubcommand>,
}

#[derive(Debug, Subcommand)]
enum MemorySubcommand {
    List,
    Search(MemorySearchArgs),
    Show {
        #[arg(value_name = "id")]
        id: String,
    },
    Forget {
        #[arg(value_name = "id")]
        id: String,
    },
    Profile,
}

#[derive(Debug, Args)]
struct MemorySearchArgs {
    #[arg(
        value_name = "query",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    query: Vec<String>,
}

#[derive(Debug, Args)]
struct DiscordArgs {
    #[command(subcommand)]
    command: Option<DiscordSubcommand>,
}

#[derive(Debug, Subcommand)]
enum DiscordSubcommand {
    Status,
    List,
    Allow {
        #[arg(value_name = "user_id")]
        user_id: u64,
    },
    Deny {
        #[arg(value_name = "user_id")]
        user_id: u64,
    },
    Autostart {
        // `bool` would default to a `--on` flag; override the action
        // so clap treats it as a positional carrying our parsed value.
        #[arg(value_name = "on|off", action = ArgAction::Set, value_parser = parse_on_off)]
        value: bool,
    },
}

#[derive(Debug, Args)]
struct WechatArgs {
    #[command(subcommand)]
    command: Option<WechatSubcommand>,
}

#[derive(Debug, Subcommand)]
enum WechatSubcommand {
    Status,
    Login,
    Logout,
    List,
    Allow {
        #[arg(value_name = "user_id")]
        user_id: String,
    },
    Deny {
        #[arg(value_name = "user_id")]
        user_id: String,
    },
    Autostart {
        #[arg(value_name = "on|off", action = ArgAction::Set, value_parser = parse_on_off)]
        value: bool,
    },
}

/// Accept the operator-friendly `on` / `off` literals (plus the more
/// explicit `true` / `false`) for `/discord autostart`. Anything else
/// errors so a typo doesn't silently flip the boot flag.
fn parse_on_off(raw: &str) -> Result<bool, String> {
    match raw.to_ascii_lowercase().as_str() {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        other => Err(format!("expected 'on' or 'off', got {other:?}")),
    }
}

impl TryFrom<RawCommand> for SlashCommand {
    type Error = String;

    fn try_from(value: RawCommand) -> Result<Self, Self::Error> {
        match value {
            RawCommand::Help => Ok(Self::Help),
            RawCommand::Skills => Ok(Self::Skills),
            RawCommand::Exit => Ok(Self::Exit),
            RawCommand::Quit => Ok(Self::Quit),
            RawCommand::New => Ok(Self::New),
            RawCommand::List => Ok(Self::List),
            RawCommand::Load { index } => Ok(Self::Load { index }),
            RawCommand::Switch(args) => args.try_into().map(Self::Switch),
            RawCommand::Compact(args) => Ok(Self::Compact {
                focus: non_empty_join(&args.focus),
            }),
            RawCommand::Heartbeat(args) => Ok(Self::Heartbeat(args.into())),
            RawCommand::Cron(args) => Ok(Self::Cron(args.into())),
            RawCommand::Memory(args) => args.try_into().map(Self::Memory),
            RawCommand::Discord(args) => Ok(Self::Discord(args.into())),
            RawCommand::Wechat(args) => Ok(Self::Wechat(args.into())),
            RawCommand::External(raw) => external_command(&raw),
        }
    }
}

impl TryFrom<SwitchArgs> for SwitchCommand {
    type Error = String;

    fn try_from(value: SwitchArgs) -> Result<Self, Self::Error> {
        match value.command {
            None => Ok(Self::List),
            Some(SwitchSubcommand::Default { profile_id: None }) => Ok(Self::ShowDefault),
            Some(SwitchSubcommand::Default {
                profile_id: Some(profile_id),
            }) => Ok(Self::SetDefault { profile_id }),
            Some(SwitchSubcommand::Runtime(raw)) => {
                let (profile_id, extra) = single_external_arg(&raw)?;
                if extra.is_empty() {
                    Ok(Self::Runtime {
                        profile_id: profile_id.clone(),
                    })
                } else {
                    Err("usage: /switch <provider/profile>".to_string())
                }
            }
        }
    }
}

impl From<HeartbeatArgs> for HeartbeatCommand {
    fn from(value: HeartbeatArgs) -> Self {
        match value.command {
            None => Self::Status,
            Some(HeartbeatSubcommand::Pause) => Self::Pause,
            Some(HeartbeatSubcommand::Resume) => Self::Resume,
            Some(HeartbeatSubcommand::Trigger) => Self::Trigger,
            Some(HeartbeatSubcommand::Interval { seconds }) => Self::Interval { seconds },
        }
    }
}

impl From<CronArgs> for CronCommand {
    fn from(value: CronArgs) -> Self {
        match value.command {
            None | Some(CronSubcommand::List) => Self::List,
            Some(CronSubcommand::Trigger { id }) => Self::Trigger { id },
            Some(CronSubcommand::Enable { id }) => Self::Enable { id },
            Some(CronSubcommand::Disable { id }) => Self::Disable { id },
            Some(CronSubcommand::Remove { id }) => Self::Remove { id },
        }
    }
}

impl TryFrom<MemoryArgs> for MemoryCommand {
    type Error = String;

    fn try_from(value: MemoryArgs) -> Result<Self, Self::Error> {
        match value.command {
            None | Some(MemorySubcommand::List) => Ok(Self::List),
            Some(MemorySubcommand::Search(args)) => non_empty_join(&args.query)
                .map(|query| Self::Search { query })
                .ok_or_else(|| "usage: /memory search <query>".to_string()),
            Some(MemorySubcommand::Show { id }) => Ok(Self::Show { id }),
            Some(MemorySubcommand::Forget { id }) => Ok(Self::Forget { id }),
            Some(MemorySubcommand::Profile) => Ok(Self::Profile),
        }
    }
}

impl From<DiscordArgs> for DiscordCommand {
    fn from(value: DiscordArgs) -> Self {
        match value.command {
            // No args toggles the connection: typing `/discord`
            // alone is the single switch — the response message
            // reports the new state so users see the result of
            // every flip without consulting `/discord status`.
            None => Self::Toggle,
            Some(DiscordSubcommand::Status) => Self::Status,
            Some(DiscordSubcommand::List) => Self::List,
            Some(DiscordSubcommand::Allow { user_id }) => Self::Allow { user_id },
            Some(DiscordSubcommand::Deny { user_id }) => Self::Deny { user_id },
            Some(DiscordSubcommand::Autostart { value }) => Self::Autostart { on: value },
        }
    }
}

impl From<WechatArgs> for WechatCommand {
    fn from(value: WechatArgs) -> Self {
        match value.command {
            None => Self::Toggle,
            Some(WechatSubcommand::Status) => Self::Status,
            Some(WechatSubcommand::Login) => Self::Login,
            Some(WechatSubcommand::Logout) => Self::Logout,
            Some(WechatSubcommand::List) => Self::List,
            Some(WechatSubcommand::Allow { user_id }) => Self::Allow { user_id },
            Some(WechatSubcommand::Deny { user_id }) => Self::Deny { user_id },
            Some(WechatSubcommand::Autostart { value }) => Self::Autostart { on: value },
        }
    }
}

fn external_command(raw: &[String]) -> Result<SlashCommand, String> {
    let Some((name, args)) = raw.split_first() else {
        return Err("usage: /<command>".to_string());
    };
    Ok(SlashCommand::External {
        name: name.clone(),
        args: args.to_vec(),
    })
}

fn single_external_arg(raw: &[String]) -> Result<(&String, &[String]), String> {
    let Some((name, args)) = raw.split_first() else {
        return Err("usage: /switch <provider/profile>".to_string());
    };
    Ok((name, args))
}

fn non_empty_join(parts: &[String]) -> Option<String> {
    let joined = parts.join(" ");
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn format_parse_error(err: &clap::Error) -> String {
    let rendered = err.to_string();
    rendered.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_and_gateway_commands() {
        assert_eq!(parse("help").unwrap(), SlashCommand::Help);
        assert_eq!(parse("quit").unwrap(), SlashCommand::Quit);
        assert_eq!(parse("new").unwrap(), SlashCommand::New);
        assert_eq!(parse("list").unwrap(), SlashCommand::List);
        assert_eq!(parse("load 2").unwrap(), SlashCommand::Load { index: 2 });
    }

    #[test]
    fn parses_switch_forms_without_prefix_collisions() {
        assert_eq!(
            parse("switch").unwrap(),
            SlashCommand::Switch(SwitchCommand::List)
        );
        assert_eq!(
            parse("switch mistral/small").unwrap(),
            SlashCommand::Switch(SwitchCommand::Runtime {
                profile_id: "mistral/small".to_string(),
            })
        );
        assert_eq!(
            parse("switch default").unwrap(),
            SlashCommand::Switch(SwitchCommand::ShowDefault)
        );
        assert_eq!(
            parse("switch default deepseek/reasoner").unwrap(),
            SlashCommand::Switch(SwitchCommand::SetDefault {
                profile_id: "deepseek/reasoner".to_string(),
            })
        );
        assert_eq!(
            parse("switcheroo").unwrap(),
            SlashCommand::External {
                name: "switcheroo".to_string(),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn rejects_extra_switch_args() {
        assert!(parse("switch a b").is_err());
        assert!(parse("switch default a b").is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn parses_agent_commands() {
        assert_eq!(
            parse("compact recent file edits").unwrap(),
            SlashCommand::Compact {
                focus: Some("recent file edits".to_string()),
            }
        );
        assert_eq!(
            parse("compact --include-tools").unwrap(),
            SlashCommand::Compact {
                focus: Some("--include-tools".to_string()),
            }
        );
        assert_eq!(
            parse("heartbeat interval 30").unwrap(),
            SlashCommand::Heartbeat(HeartbeatCommand::Interval { seconds: 30 })
        );
        assert_eq!(
            parse("cron trigger job-1").unwrap(),
            SlashCommand::Cron(CronCommand::Trigger {
                id: "job-1".to_string(),
            })
        );
        assert_eq!(
            parse("memory").unwrap(),
            SlashCommand::Memory(MemoryCommand::List)
        );
        assert_eq!(
            parse("memory search response style").unwrap(),
            SlashCommand::Memory(MemoryCommand::Search {
                query: "response style".to_string(),
            })
        );
        assert_eq!(
            parse("memory show mem-1").unwrap(),
            SlashCommand::Memory(MemoryCommand::Show {
                id: "mem-1".to_string(),
            })
        );
        assert_eq!(
            parse("memory forget mem-1").unwrap(),
            SlashCommand::Memory(MemoryCommand::Forget {
                id: "mem-1".to_string(),
            })
        );
        assert_eq!(
            parse("memory profile").unwrap(),
            SlashCommand::Memory(MemoryCommand::Profile)
        );
        assert!(parse("memory search").is_err());
        assert_eq!(
            parse("discord").unwrap(),
            SlashCommand::Discord(DiscordCommand::Toggle)
        );
        assert_eq!(
            parse("discord status").unwrap(),
            SlashCommand::Discord(DiscordCommand::Status)
        );
        // `enable` / `disable` were folded into the toggle; the
        // parser must reject them so a stale habit doesn't silently
        // map to the External fallback.
        assert!(parse("discord enable").is_err());
        assert!(parse("discord disable").is_err());
        assert_eq!(
            parse("discord autostart on").unwrap(),
            SlashCommand::Discord(DiscordCommand::Autostart { on: true })
        );
        assert_eq!(
            parse("discord autostart off").unwrap(),
            SlashCommand::Discord(DiscordCommand::Autostart { on: false })
        );
        assert!(parse("discord autostart maybe").is_err());
        assert_eq!(
            parse("discord list").unwrap(),
            SlashCommand::Discord(DiscordCommand::List)
        );
        assert_eq!(
            parse("discord allow 123456789012345678").unwrap(),
            SlashCommand::Discord(DiscordCommand::Allow {
                user_id: 123_456_789_012_345_678,
            })
        );
        assert_eq!(
            parse("discord deny 42").unwrap(),
            SlashCommand::Discord(DiscordCommand::Deny { user_id: 42 })
        );
        assert!(parse("discord allow not-a-number").is_err());
        assert!(parse("discord allow").is_err());
        assert_eq!(
            parse("wechat").unwrap(),
            SlashCommand::Wechat(WechatCommand::Toggle)
        );
        assert_eq!(
            parse("wechat login").unwrap(),
            SlashCommand::Wechat(WechatCommand::Login)
        );
        assert_eq!(
            parse("wechat logout").unwrap(),
            SlashCommand::Wechat(WechatCommand::Logout)
        );
        assert_eq!(
            parse("wechat status").unwrap(),
            SlashCommand::Wechat(WechatCommand::Status)
        );
        assert_eq!(
            parse("wechat allow wxid_test").unwrap(),
            SlashCommand::Wechat(WechatCommand::Allow {
                user_id: "wxid_test".to_string()
            })
        );
        assert_eq!(
            parse("wechat deny wxid_test").unwrap(),
            SlashCommand::Wechat(WechatCommand::Deny {
                user_id: "wxid_test".to_string()
            })
        );
        assert_eq!(
            parse("wechat autostart on").unwrap(),
            SlashCommand::Wechat(WechatCommand::Autostart { on: true })
        );
        assert!(parse("wechat autostart maybe").is_err());
    }

    #[test]
    fn preserves_unknown_commands_for_skill_fallback() {
        assert_eq!(
            parse("draft arg1 arg2").unwrap(),
            SlashCommand::External {
                name: "draft".to_string(),
                args: vec!["arg1".to_string(), "arg2".to_string()],
            }
        );
    }
}
