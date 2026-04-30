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
    /// - [`ConfigError::Write`] / [`ConfigError::Serialize`] if the file
    ///   parsed successfully but needed missing default fields backfilled.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let raw: toml::Value = toml::from_str(&text)?;
        let mut cfg: AppConfig = toml::from_str(&text)?;
        cfg.source_path = path.to_path_buf();
        cfg.validate()?;
        if needs_default_backfill(&raw, &cfg)? {
            cfg.save()?;
        }
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
    /// `bootstrap::interactive` and [`Self::save`]:
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

    /// Persist this configuration to `source_path`.
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
    /// saves happen only during bootstrap, explicit runtime config
    /// mutations, and default-field backfills. When richer config
    /// mutation lands, swap in `toml_edit` to preserve formatting.
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
    /// 4. `agent.heartbeat.interval_secs` is greater than zero.
    /// 5. `agent.memory.snapshot_limit` is greater than zero.
    /// 6. `agent.dream.schedule` parses and Dream budgets/limits are non-zero.
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

        if self.agent.heartbeat.interval_secs == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.heartbeat.interval_secs",
                reason: "must be greater than zero".into(),
            });
        }

        if self.agent.memory.snapshot_limit == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.memory.snapshot_limit",
                reason: "must be greater than zero".into(),
            });
        }

        crate::cron::Schedule::cron(&self.agent.dream.schedule).map_err(|err| {
            ConfigError::Invalid {
                field: "agent.dream.schedule",
                reason: err.to_string(),
            }
        })?;
        if self.agent.dream.min_interval_secs == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.min_interval_secs",
                reason: "must be greater than zero".into(),
            });
        }
        if self.agent.dream.lock_stale_secs == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.lock_stale_secs",
                reason: "must be greater than zero".into(),
            });
        }
        if self.agent.dream.min_sessions_per_run < 5 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.min_sessions_per_run",
                reason: "must be at least 5".into(),
            });
        }
        if self.agent.dream.max_events_per_run == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.max_events_per_run",
                reason: "must be greater than zero".into(),
            });
        }
        if self.agent.dream.max_prompt_chars == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.max_prompt_chars",
                reason: "must be greater than zero".into(),
            });
        }
        if self.agent.dream.max_output_tokens == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.max_output_tokens",
                reason: "must be greater than zero".into(),
            });
        }
        if self.agent.dream.max_event_chars == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.max_event_chars",
                reason: "must be greater than zero".into(),
            });
        }
        if self.agent.dream.max_existing_memories == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.max_existing_memories",
                reason: "must be greater than zero".into(),
            });
        }
        if self.agent.dream.max_candidates == 0 {
            return Err(ConfigError::Invalid {
                field: "agent.dream.max_candidates",
                reason: "must be greater than zero".into(),
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

fn needs_default_backfill(raw: &toml::Value, cfg: &AppConfig) -> Result<bool> {
    let canonical_text = toml::to_string_pretty(cfg)?;
    let canonical: toml::Value = toml::from_str(&canonical_text)?;
    Ok(has_missing_default_keys(&canonical, raw))
}

fn has_missing_default_keys(canonical: &toml::Value, raw: &toml::Value) -> bool {
    let (toml::Value::Table(canonical), toml::Value::Table(raw)) = (canonical, raw) else {
        return false;
    };
    canonical.iter().any(|(key, canonical_value)| {
        raw.get(key)
            .is_none_or(|raw_value| has_missing_default_keys(canonical_value, raw_value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{AgentConfig, LLMConfig, LLMProfile, TuiConfig};
    use std::collections::HashMap;
    use uuid::Uuid;

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
            sandbox: crate::security::SandboxConfig::default(),
            channels: crate::config::ChannelsConfig::default(),
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
        assert!(parsed.agent.memory.enabled);
        assert!(parsed.agent.memory.session_snapshot);
        assert!(parsed.agent.memory.profile_enabled);
        assert_eq!(parsed.agent.memory.snapshot_limit, 8);
        assert_eq!(parsed.agent.dream.lock_stale_secs, 21_600);
        assert_eq!(parsed.agent.dream.min_sessions_per_run, 5);
        assert_eq!(parsed.agent.dream.max_event_chars, 2_000);
        assert_eq!(parsed.agent.dream.max_existing_memories, 24);
        assert_eq!(parsed.agent.dream.max_candidates, 8);
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

    #[test]
    fn from_file_rejects_zero_heartbeat_interval() {
        let path = std::env::temp_dir().join(format!("mandeven-config-{}.toml", Uuid::now_v7()));
        let text = r#"
            [llm]
            default = "acme/my-profile"

            [llm.acme.my-profile]
            model_name = "upstream-model"
            max_context_window = 128000

            [agent.heartbeat]
            interval_secs = 0
        "#;
        std::fs::write(&path, text).unwrap();

        let err = AppConfig::from_file(&path).unwrap_err().to_string();

        assert!(err.contains("agent.heartbeat.interval_secs"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn from_file_rejects_zero_memory_snapshot_limit() {
        let path = std::env::temp_dir().join(format!("mandeven-config-{}.toml", Uuid::now_v7()));
        let text = r#"
            [llm]
            default = "acme/my-profile"

            [llm.acme.my-profile]
            model_name = "upstream-model"
            max_context_window = 128000

            [agent.memory]
            snapshot_limit = 0
        "#;
        std::fs::write(&path, text).unwrap();

        let err = AppConfig::from_file(&path).unwrap_err().to_string();

        assert!(err.contains("agent.memory.snapshot_limit"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn from_file_rejects_invalid_dream_schedule() {
        let path = std::env::temp_dir().join(format!("mandeven-config-{}.toml", Uuid::now_v7()));
        let text = r#"
            [llm]
            default = "acme/my-profile"

            [llm.acme.my-profile]
            model_name = "upstream-model"
            max_context_window = 128000

            [agent.dream]
            schedule = "not a cron"
        "#;
        std::fs::write(&path, text).unwrap();

        let err = AppConfig::from_file(&path).unwrap_err().to_string();

        assert!(err.contains("agent.dream.schedule"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn from_file_backfills_missing_default_fields() {
        let path = std::env::temp_dir().join(format!("mandeven-config-{}.toml", Uuid::now_v7()));
        let text = r#"
            [llm]
            default = "acme/my-profile"

            [llm.acme.my-profile]
            model_name = "upstream-model"
            max_context_window = 128000

            [agent.dream]
            schedule = "0 4 * * *"
        "#;
        std::fs::write(&path, text).unwrap();

        let cfg = AppConfig::from_file(&path).unwrap();
        let updated = std::fs::read_to_string(&path).unwrap();

        assert_eq!(cfg.agent.dream.schedule, "0 4 * * *");
        assert_eq!(cfg.agent.dream.lock_stale_secs, 21_600);
        assert!(updated.contains("schedule = \"0 4 * * *\""));
        assert!(updated.contains("lock_stale_secs = 21600"));
        assert!(updated.contains("min_sessions_per_run = 5"));
        assert!(updated.contains("max_candidates = 8"));
        assert!(updated.contains("snapshot_limit = 8"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn from_file_preserves_existing_values_while_backfilling() {
        let path = std::env::temp_dir().join(format!("mandeven-config-{}.toml", Uuid::now_v7()));
        let text = r#"
            [llm]
            default = "acme/my-profile"

            [llm.acme.my-profile]
            model_name = "upstream-model"
            max_context_window = 128000

            [agent.memory]
            snapshot_limit = 3

            [agent.dream]
            schedule = "0 4 * * *"
            lock_stale_secs = 99
        "#;
        std::fs::write(&path, text).unwrap();

        let cfg = AppConfig::from_file(&path).unwrap();
        let updated = std::fs::read_to_string(&path).unwrap();

        assert_eq!(cfg.agent.memory.snapshot_limit, 3);
        assert_eq!(cfg.agent.dream.lock_stale_secs, 99);
        assert!(updated.contains("snapshot_limit = 3"));
        assert!(updated.contains("lock_stale_secs = 99"));
        assert!(updated.contains("max_event_chars = 2000"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn from_file_rejects_zero_dream_limits() {
        for field in [
            "lock_stale_secs",
            "max_event_chars",
            "max_existing_memories",
            "max_candidates",
        ] {
            let path =
                std::env::temp_dir().join(format!("mandeven-config-{}.toml", Uuid::now_v7()));
            let text = format!(
                r#"
                    [llm]
                    default = "acme/my-profile"

                    [llm.acme.my-profile]
                    model_name = "upstream-model"
                    max_context_window = 128000

                    [agent.dream]
                    {field} = 0
                "#
            );
            std::fs::write(&path, text).unwrap();

            let err = AppConfig::from_file(&path).unwrap_err().to_string();

            assert!(err.contains(&format!("agent.dream.{field}")));
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn from_file_rejects_dream_min_sessions_below_five() {
        let path = std::env::temp_dir().join(format!("mandeven-config-{}.toml", Uuid::now_v7()));
        let text = r#"
            [llm]
            default = "acme/my-profile"

            [llm.acme.my-profile]
            model_name = "upstream-model"
            max_context_window = 128000

            [agent.dream]
            min_sessions_per_run = 4
        "#;
        std::fs::write(&path, text).unwrap();

        let err = AppConfig::from_file(&path).unwrap_err().to_string();

        assert!(err.contains("agent.dream.min_sessions_per_run"));
        let _ = std::fs::remove_file(path);
    }
}
