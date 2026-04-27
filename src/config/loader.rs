//! Config file discovery, parsing, and structural validation.

use std::fs;
use std::path::{Path, PathBuf};

use super::bootstrap;
use super::error::{ConfigError, Result};
use super::paths;
use super::types::AppConfig;

/// Separator between provider and model name in `LLMConfig::default`.
const DEFAULT_MODEL_SEPARATOR: char = '/';

/// Suffix appended to the target path for the temp file used by the
/// `write → rename` atomic save. Kept short and POSIX-friendly.
const TMP_SAVE_SUFFIX: &str = ".tmp";

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

    /// Load from the per-user config file at [`paths::config_path`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NotFound`] if the file is absent; see
    /// [`Self::from_file`] for the remaining failure modes once the
    /// file is located.
    pub fn load() -> Result<Self> {
        let path = paths::config_path();
        if !path.exists() {
            return Err(ConfigError::NotFound(path));
        }
        Self::from_file(path)
    }

    /// Canonical entry point for `main`: load the per-user config when
    /// it exists, otherwise run the interactive first-run bootstrap
    /// and persist the result to [`paths::config_path`].
    ///
    /// # Errors
    ///
    /// Surfaces every failure mode of [`Self::load`], plus those from
    /// [`bootstrap::interactive`] and [`Self::save`]:
    /// [`ConfigError::NotInteractive`] when stdin is not a tty,
    /// [`ConfigError::Aborted`] on Ctrl-D during the prompt,
    /// [`ConfigError::Io`] on terminal I/O failures, and
    /// [`ConfigError::Write`] / [`ConfigError::Serialize`] when
    /// persisting the new config fails.
    pub fn bootstrap() -> Result<Self> {
        let path = paths::config_path();
        if path.exists() {
            return Self::from_file(path);
        }
        let mut cfg = bootstrap::interactive()?;
        cfg.source_path = path;
        cfg.save()?;
        Ok(cfg)
    }

    /// Persist this configuration to [`Self::source_path`].
    ///
    /// Writes via the standard `write → rename` pattern: TOML text is
    /// serialized in full, written to `<path>.tmp`, then `rename`-d
    /// into place. POSIX guarantees the rename is atomic, so a reader
    /// observes either the old file or the fully-written new file —
    /// never a torn state.
    ///
    /// Creates the parent directory recursively when missing — the
    /// canonical bootstrap path writes to `~/.mandeven/mandeven.toml`
    /// on a host that has never run mandeven before, so the
    /// `~/.mandeven/` directory itself often does not exist yet.
    ///
    /// Comment preservation is not handled; a `save()` that was
    /// preceded by a user-edited file will round-trip the semantic
    /// values but drop comments. This is acceptable for now because
    /// the only caller is [`Self::bootstrap`], which writes a fresh
    /// file. When interactive config mutation lands, swap in
    /// `toml_edit` to preserve formatting.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Serialize`] when `AppConfig` cannot be rendered
    ///   as TOML (shouldn't happen for a validated config, but the
    ///   serde contract surfaces the error).
    /// - [`ConfigError::Write`] on any underlying I/O failure (parent
    ///   `mkdir`, write, or rename).
    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(self)?;

        let path = &self.source_path;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut tmp = path.as_os_str().to_owned();
        tmp.push(TMP_SAVE_SUFFIX);
        let tmp = PathBuf::from(tmp);

        fs::write(&tmp, text).map_err(|source| ConfigError::Write {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| ConfigError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{AgentConfig, LLMConfig, LLMProfile, TuiConfig};
    use std::collections::HashMap;

    /// Toml + serde on a doubly-flattened `HashMap` layout (the
    /// provider → model nesting is entirely `#[serde(flatten)]`) has
    /// historically been the brittle spot in this schema. Locking the
    /// serialize → deserialize round-trip here guards against a future
    /// toml / serde upgrade silently breaking `AppConfig::save`.
    #[test]
    fn app_config_round_trips_through_toml() {
        let mut models = HashMap::new();
        models.insert(
            "my-profile".to_string(),
            LLMProfile {
                model_name: "upstream-model".into(),
                max_context_window: 128_000,
                max_tokens: Some(2048),
                temperature: None,
                thinking: Some(true),
            },
        );
        let mut providers = HashMap::new();
        providers.insert("acme".to_string(), models);

        let original = AppConfig {
            llm: LLMConfig {
                default: "acme/my-profile".into(),
                timeout_secs: Some(45),
                providers,
            },
            tui: TuiConfig {
                show_thinking: false,
            },
            agent: AgentConfig::default(),
            source_path: std::path::PathBuf::new(),
        };

        let serialized = toml::to_string_pretty(&original).expect("serialize");
        let parsed: AppConfig = toml::from_str(&serialized).expect("deserialize");

        assert_eq!(parsed.llm.default, "acme/my-profile");
        assert_eq!(parsed.llm.timeout_secs, Some(45));
        let acme = parsed.llm.providers.get("acme").expect("provider present");
        let prof = acme.get("my-profile").expect("profile present");
        assert_eq!(prof.model_name, "upstream-model");
        assert_eq!(prof.max_context_window, 128_000);
        assert_eq!(prof.max_tokens, Some(2048));
        assert_eq!(prof.temperature, None);
        assert!(!parsed.tui.show_thinking);
    }

    #[test]
    fn tui_config_hides_thinking_by_default() {
        let text = r#"
            [llm]
            default = "acme/my-profile"

            [llm.acme.my-profile]
            model_name = "upstream-model"
            max_context_window = 128000
        "#;

        let parsed: AppConfig = toml::from_str(text).expect("deserialize");

        assert!(!parsed.tui.show_thinking);
    }
}
