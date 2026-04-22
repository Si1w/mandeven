//! Configuration data types deserialized from `./mandeven.toml`.
//!
//! Top-level sections are added here as the corresponding modules start
//! needing user-tunable values. Fields that are internal invariants live
//! as `const` in their owning module, not here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Root configuration loaded from `./mandeven.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    /// LLM profile catalog.
    pub llm: LLMConfig,

    /// Agent-loop configuration. Entire section is optional in TOML;
    /// missing fields fall back to [`AgentConfig::default`].
    #[serde(default)]
    pub agent: AgentConfig,

    /// Filesystem path this config was loaded from.
    ///
    /// Populated by [`AppConfig::from_file`] and [`AppConfig::load`];
    /// empty for in-memory construction. Not serialized — only used to
    /// derive runtime data locations via [`AppConfig::data_dir`].
    #[serde(skip)]
    pub(crate) source_path: PathBuf,
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
}

impl AppConfig {
    /// Instance-level data directory for this config.
    ///
    /// Follows the convention that the data directory is the parent
    /// directory of the config file. Session, cron, and log subdirectories
    /// are derived from this root by downstream modules.
    ///
    /// Falls back to the current working directory (`.`) when the source
    /// path has no parent (for example, when it is empty or `"/"`).
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.source_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    }
}

/// LLM profile catalog, grouped by provider.
///
/// The corresponding TOML layout is:
///
/// ```toml
/// [llm]
/// default = "mistral/small"
///
/// [llm.mistral.small]
/// model_name         = "mistral-small-latest"
/// max_context_window = 256000
/// max_tokens         = 4096
/// temperature        = 0.7
/// timeout_secs       = 30
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

    /// Provider name -> user-chosen model name -> per-profile settings.
    #[serde(flatten)]
    pub providers: HashMap<String, HashMap<String, LLMProfile>>,
}

/// One user-named profile under a provider.
///
/// The enclosing provider supplies `base_url` and `api_key_env`; this
/// struct only carries the upstream model identifier plus the per-profile
/// tuning parameters.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LLMProfile {
    /// Model identifier sent in the API request body
    /// (for example `"mistral-small-latest"`).
    pub model_name: String,

    /// Maximum context window of this model, in tokens.
    pub max_context_window: u32,

    /// Upper bound on completion tokens per request.
    pub max_tokens: u32,

    /// Sampling temperature. Valid range: `[0.0, 2.0]`.
    pub temperature: f32,

    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
}
