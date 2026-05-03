//! Configuration loading from `~/.mandeven/mandeven.toml`.
//!
//! The canonical entry points are [`AppConfig::load`] (read the
//! per-user config file) and [`AppConfig::from_file`] (explicit path,
//! used by tests and migration tools). New top-level sections are
//! added to [`AppConfig`] as the corresponding modules start needing
//! user-tunable values.
//!
//! See [`paths`] for the on-disk layout — the same module owns every
//! well-known path under [`paths::home_dir`].

mod bootstrap;
pub mod error;
pub mod loader;
pub mod paths;
pub mod types;

pub use error::{ConfigError, Result};
pub use paths::{
    CONFIG_FILENAME, CRON_BUCKET_NAME, HOME_ENV_VAR, HOME_SUBDIR, PROJECTS_SUBDIR, config_path,
    cron_bucket, home_dir, project_bucket, projects_dir,
};
pub use types::{
    AgentConfig, AppConfig, ChannelsConfig, DiscordConfig, LLMConfig, LLMProfile, TuiConfig,
    WechatConfig,
};
