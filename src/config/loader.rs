//! Config file discovery, parsing, and structural validation.

use std::fs;
use std::path::{Path, PathBuf};

use super::error::{ConfigError, Result};
use super::types::AppConfig;

/// File name looked up in the current working directory by [`AppConfig::load`].
const DEFAULT_CONFIG_FILENAME: &str = "mandeven.toml";

/// Separator between provider and model name in `LLMConfig::default`.
const DEFAULT_MODEL_SEPARATOR: char = '/';

impl AppConfig {
    /// Load configuration from an explicit file path.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - [`ConfigError::Read`] if the file cannot be opened or read.
    /// - [`ConfigError::Parse`] if the contents are not valid TOML or do
    ///   not match the schema.
    /// - [`ConfigError::Invalid`] if the parsed values fail structural
    ///   validation.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut cfg: AppConfig = toml::from_str(&text)?;
        cfg.source_path = path.to_path_buf();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from `./mandeven.toml` in the current working directory.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NotFound`] if the file is absent; see
    /// [`Self::from_file`] for the remaining failure modes once the
    /// file is located.
    pub fn load() -> Result<Self> {
        let path = PathBuf::from(DEFAULT_CONFIG_FILENAME);
        if !path.exists() {
            return Err(ConfigError::NotFound);
        }
        Self::from_file(path)
    }

    /// Persist this configuration to `./mandeven.toml`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written or serialization fails.
    pub fn save(&self) -> Result<()> {
        todo!("atomic write-back; consider toml_edit for comment preservation")
    }

    /// Structural invariants checked after parsing:
    ///
    /// 1. `llm.providers` is non-empty.
    /// 2. `llm.default` parses as `"provider/model"` with exactly one `/`
    ///    and non-empty sides.
    /// 3. The referenced provider and model both exist in `providers`.
    fn validate(&self) -> Result<()> {
        if self.llm.providers.is_empty() {
            return Err(ConfigError::Invalid {
                field: "llm.providers",
                reason: "no providers declared".into(),
            });
        }

        let (provider, model) = split_default(&self.llm.default)?;

        let models = self
            .llm
            .providers
            .get(provider)
            .ok_or_else(|| ConfigError::Invalid {
                field: "llm.default",
                reason: format!("provider '{provider}' not declared"),
            })?;

        if !models.contains_key(model) {
            return Err(ConfigError::Invalid {
                field: "llm.default",
                reason: format!("model '{model}' not declared under provider '{provider}'"),
            });
        }

        Ok(())
    }
}

/// Split `"provider/model"` into its two components.
fn split_default(raw: &str) -> Result<(&str, &str)> {
    let invalid = || ConfigError::Invalid {
        field: "llm.default",
        reason: format!("expected 'provider/model', got '{raw}'"),
    };

    let (provider, model) = raw
        .split_once(DEFAULT_MODEL_SEPARATOR)
        .ok_or_else(invalid)?;

    if provider.is_empty() || model.is_empty() || model.contains(DEFAULT_MODEL_SEPARATOR) {
        return Err(invalid());
    }

    Ok((provider, model))
}
