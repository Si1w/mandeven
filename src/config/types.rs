//! Configuration data types deserialized from `~/.mandeven/mandeven.toml`.
//!
//! Top-level sections are added here as the corresponding modules start
//! needing user-tunable values. Fields that are internal invariants live
//! as `const` in their owning module, not here.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::paths;
use crate::agent::compact::CompactConfig;
use crate::hook::HookConfig;
use crate::memory::MemoryConfig;
use crate::security::SandboxConfig;
use crate::skill::SkillConfig;

/// Root configuration loaded from `~/.mandeven/mandeven.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    /// LLM profile catalog.
    pub llm: LLMConfig,

    /// Terminal UI preferences. Entire section is optional in TOML;
    /// missing fields preserve the current UI defaults.
    #[serde(default)]
    pub tui: TuiConfig,

    /// Agent-loop configuration. Entire section is optional in TOML;
    /// missing fields fall back to [`AgentConfig::default`].
    #[serde(default)]
    pub agent: AgentConfig,

    /// Sandbox capability tier shared across every tool. Optional in
    /// TOML; missing section defaults to
    /// [`crate::security::SandboxPolicy::WorkspaceWrite`].
    #[serde(default)]
    pub sandbox: SandboxConfig,

    /// External channel adapters. Optional in TOML; missing section
    /// leaves every adapter disabled and only the local TUI channel
    /// is registered.
    #[serde(default)]
    pub channels: ChannelsConfig,

    /// Filesystem path this config was loaded from. Populated by
    /// [`AppConfig::from_file`] / [`AppConfig::load`], empty for
    /// in-memory construction. Diagnostic only — runtime data
    /// directories resolve through [`paths::home_dir`] regardless.
    #[serde(skip)]
    pub(crate) source_path: PathBuf,
}

/// External / network channel adapters.
///
/// Every field is optional so users can enable channels one at a time
/// without listing the rest. A missing or `enabled = false` block
/// keeps the adapter from being registered at boot.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChannelsConfig {
    /// Discord adapter. See [`DiscordConfig`].
    pub discord: Option<DiscordConfig>,
    /// `WeChat` personal-account adapter. See [`WechatConfig`].
    pub wechat: Option<WechatConfig>,
}

/// Discord adapter configuration.
///
/// MS0 scope: DM-only with a runtime-mutable allowlist + runtime
/// toggle. Server / guild channels are not supported.
///
/// Section semantics:
/// - **Section absent** ⇒ Discord adapter is not registered at all
///   and `/discord ...` returns a "not configured" notice.
/// - **Section present** ⇒ adapter is always registered; `enabled`
///   only chooses whether the gateway connection is opened at boot.
///   `/discord` toggles the connection at runtime without a restart.
///
/// The allowed-user list and the resolved bot token never live here;
/// see [`crate::channels::discord`] for where they are persisted.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscordConfig {
    /// Auto-start the gateway connection at boot. Defaults to
    /// `false` so `[channels.discord]` (empty body) registers the
    /// adapter without auto-connecting — the user runs `/discord`
    /// when ready. Setting `true` connects on launch.
    #[serde(default)]
    pub enabled: bool,

    /// Name of the environment variable that holds the bot token.
    /// Defaults to `DISCORD_BOT_TOKEN`. The token itself never lives
    /// in `mandeven.toml` so the file stays safe to commit. Re-read
    /// on every `/discord` start so token rotation works without
    /// a restart.
    #[serde(default = "default_discord_token_env")]
    pub token_env: String,
}

fn default_discord_token_env() -> String {
    "DISCORD_BOT_TOKEN".to_string()
}

/// Personal `WeChat` adapter configuration.
///
/// MS0 scope: text-only DMs via Tencent's iLink Bot API. The token
/// produced by QR login is stored under
/// `~/.mandeven/channels/wechat/accounts/`; `mandeven.toml` only keeps
/// non-sensitive runtime preferences and environment-variable names.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WechatConfig {
    /// Auto-start the iLink long-poll connection at boot. Defaults to
    /// `false`; users can run `/wechat` after logging in.
    #[serde(default)]
    pub enabled: bool,

    /// Environment variable holding the iLink bot token. Defaults to
    /// `WECHAT_TOKEN`; `WEIXIN_TOKEN` is also read as a compatibility
    /// fallback by the runtime resolver.
    #[serde(default = "default_wechat_token_env")]
    pub token_env: String,

    /// Environment variable holding the iLink account id. Defaults to
    /// `WECHAT_ACCOUNT_ID`; `WEIXIN_ACCOUNT_ID` is also read as a
    /// compatibility fallback by the runtime resolver.
    #[serde(default = "default_wechat_account_id_env")]
    pub account_id_env: String,

    /// Base URL for the iLink Bot API. Defaults to Tencent's public
    /// endpoint; QR login may return a redirected base URL, which is
    /// stored with the account credentials.
    #[serde(default = "default_wechat_base_url")]
    pub base_url: String,

    /// QR login timeout in seconds.
    #[serde(default = "default_wechat_login_timeout_secs")]
    pub login_timeout_secs: u64,
}

fn default_wechat_token_env() -> String {
    "WECHAT_TOKEN".to_string()
}

fn default_wechat_account_id_env() -> String {
    "WECHAT_ACCOUNT_ID".to_string()
}

fn default_wechat_base_url() -> String {
    "https://ilinkai.weixin.qq.com".to_string()
}

fn default_wechat_login_timeout_secs() -> u64 {
    480
}

/// Terminal UI preferences.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TuiConfig {
    /// Render model reasoning traces in the transcript when providers
    /// return them. This only affects display; reasoning remains in
    /// session history so changing the setting later can reveal it.
    pub show_thinking: bool,
}

/// Agent-loop configuration.
///
/// Loop-level knobs only. Prompt content (system prompt, title prompt,
/// etc.) is owned by the future `prompts` module, not here.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentConfig {
    /// Maximum LLM iterations within a single user turn.
    ///
    /// When `None`, the inner loop has no upper bound and runs until
    /// the model stops invoking tools. Each iteration corresponds to
    /// one LLM call plus any tool dispatch it triggers.
    pub max_iterations: Option<u8>,

    /// Conversation-compaction configuration. Sets the auto-trigger
    /// thresholds, preserve-region budgets, and circuit-breaker
    /// limits used by [`crate::agent::compact`]. All fields are
    /// percent-of-window so a single config scales across providers
    /// with very different `max_context_window` values.
    #[serde(default)]
    pub compact: CompactConfig,

    /// Per-agent skill configuration. Just the on/off switch —
    /// skill definitions live in `~/.mandeven/skills/<name>/SKILL.md`
    /// and the runtime index can re-read them on access.
    #[serde(default)]
    pub skill: SkillConfig,

    /// Per-agent hook configuration. Just the on/off switch — hook
    /// definitions live in `~/.mandeven/hooks.json`, are loaded at
    /// boot, and are reloaded on hook events after the file changes.
    #[serde(default)]
    pub hook: HookConfig,

    /// Per-agent memory configuration. Memory lives in the single editable
    /// `~/.mandeven/MEMORY.md` file and is injected as transient user context.
    #[serde(default)]
    pub memory: MemoryConfig,
}

impl AppConfig {
    /// Per-user data directory.
    ///
    /// Always resolves through [`paths::home_dir`] (i.e.
    /// `$MANDEVEN_HOME` if set, else `~/.mandeven/`). All
    /// agent-managed state — `AGENTS.md`, `MEMORY.md`, global timer state,
    /// editable skills, and per-project `projects/<bucket>/`
    /// session/task/timer directories — lives under this root.
    ///
    /// Independent of `source_path`: the config file location is for
    /// diagnostics only. This guarantees `data_dir()` is consistent
    /// even when the config has been hand-loaded from an unusual path
    /// (tests, migrations).
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        paths::home_dir()
    }
}

/// LLM profile catalog, grouped by provider.
///
/// The corresponding TOML layout is:
///
/// ```toml
/// [llm]
/// default = "mistral/mistral-small"
/// # timeout_secs = 30   # optional; applies to every provider call
///
/// [llm.mistral.mistral-small]
/// model_name         = "mistral-small-latest"
/// max_context_window = 256000
/// # max_tokens / temperature are optional; omit to let the provider
/// # API apply its own defaults.
/// ```
///
/// Provider names (`mistral`, `groq`, ...) must match an entry in
/// `llm::providers`. That membership check is performed by the `llm`
/// module when a profile is dialed, not here.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LLMConfig {
    /// Qualified name `"provider/model"` identifying the default profile.
    /// Must point to an existing entry inside `providers`.
    pub default: String,

    /// Per-request HTTP timeout in seconds, shared across every
    /// provider call. Belongs here rather than on [`LLMProfile`]
    /// because it is a transport concern, not a per-model tuning
    /// parameter. `None` disables the local timeout — requests run
    /// until the remote closes or the process is interrupted.
    pub timeout_secs: Option<u64>,

    /// Provider name -> user-chosen model name -> per-profile settings.
    #[serde(flatten)]
    pub providers: HashMap<String, HashMap<String, LLMProfile>>,
}

/// One user-named profile under a provider.
///
/// The enclosing provider supplies `base_url` and `api_key_env`; this
/// struct only carries the upstream model identifier plus the per-profile
/// tuning parameters. Sampling parameters are optional so the provider
/// API can apply its own defaults when the user does not specify one —
/// we deliberately do not inject client-side fallbacks.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LLMProfile {
    /// Model identifier sent in the API request body
    /// (for example `"mistral-small-latest"`).
    pub model_name: String,

    /// Maximum context window of this model, in tokens.
    pub max_context_window: u32,

    /// Upper bound on completion tokens per request. `None` lets the
    /// provider apply its own default.
    pub max_tokens: Option<u32>,

    /// Sampling temperature. Valid range: `[0.0, 2.0]`. `None` lets
    /// the provider apply its own default.
    pub temperature: Option<f32>,

    /// Whether to ask the model for its chain-of-thought trace.
    /// `None` leaves the field unset and lets the provider apply its
    /// per-model default (`DeepSeek` defaults `extra_body.thinking`
    /// to `enabled` on thinking-capable models). Set `Some(true)` to
    /// force-enable, `Some(false)` to force-disable. Providers
    /// without thinking support ignore the field entirely.
    pub thinking: Option<bool>,
}
