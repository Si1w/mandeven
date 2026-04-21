//! Configuration loading from `./mandeven.toml`.
//!
//! The canonical entry points are [`AppConfig::load`] (read
//! `./mandeven.toml` from the current working directory) and
//! [`AppConfig::from_file`] (explicit path). New top-level sections are
//! added to [`AppConfig`] as the corresponding modules start needing
//! user-tunable values.

pub mod error;
pub mod loader;
pub mod types;

pub use error::{ConfigError, Result};
pub use types::{AppConfig, LLMConfig, LLMProfile};
