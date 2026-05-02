//! Unified on-disk persistence for timer state.
//!
//! Timers are machine state, not user-visible work documents. The
//! store is a single JSON file under `~/.mandeven/timers.json`; task
//! descriptions and status remain in the project-local task Markdown
//! store.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::{Error, Result};
use super::{Schedule, Timer, TimerTargetRef};

/// Global timer store filename under `~/.mandeven/`.
pub const GLOBAL_TIMER_FILENAME: &str = "timers.json";

/// In-memory timer store shape.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StoreFile {
    /// Timers in insertion order.
    #[serde(default)]
    pub timers: Vec<Timer>,
}

/// Async I/O wrapper around the global JSON timer file.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// Construct a store rooted at `<data_dir>/timers.json`.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(GLOBAL_TIMER_FILENAME),
        }
    }

    /// Path to the canonical timer JSON file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load timer JSON. Missing file is an empty store.
    ///
    /// # Errors
    ///
    /// Returns timer store I/O, JSON, or schema validation errors.
    pub async fn load(&self) -> Result<StoreFile> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(raw) => {
                let (file, migrated) = decode_store_file(&raw)?;
                if migrated {
                    self.save(&file).await?;
                }
                Ok(file)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(err) => Err(err.into()),
        }
    }

    /// Save timer JSON.
    ///
    /// # Errors
    ///
    /// Returns timer store I/O, JSON, or schema validation errors.
    pub async fn save(&self, file: &StoreFile) -> Result<()> {
        validate_store_file(file)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let raw = serde_json::to_string_pretty(file)?;
        tokio::fs::write(&self.path, format!("{raw}\n")).await?;
        Ok(())
    }
}

fn decode_store_file(raw: &str) -> Result<(StoreFile, bool)> {
    match serde_json::from_str::<StoreFile>(raw) {
        Ok(file) => {
            validate_store_file(&file)?;
            Ok((file, false))
        }
        Err(new_shape_err) => match serde_json::from_str::<LegacyStoreFile>(raw) {
            Ok(legacy) => legacy.into_store_file().map(|file| (file, true)),
            Err(_) => Err(new_shape_err.into()),
        },
    }
}

fn validate_store_file(file: &StoreFile) -> Result<()> {
    for timer in &file.timers {
        if Uuid::parse_str(&timer.id).is_err() {
            return Err(Error::InvalidStore(format!(
                "timer id must be a UUID: {}",
                timer.id
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct LegacyStoreFile {
    #[serde(default)]
    timers: Vec<LegacySkillTimer>,
}

impl LegacyStoreFile {
    fn into_store_file(self) -> Result<StoreFile> {
        let now = Utc::now();
        let timers = self
            .timers
            .into_iter()
            .map(|timer| timer.into_timer(now))
            .collect::<Result<Vec<_>>>()?;
        Ok(StoreFile { timers })
    }
}

#[derive(Debug, Deserialize)]
struct LegacySkillTimer {
    kind: String,
    skill: String,
    #[serde(default = "default_true")]
    enabled: bool,
    expr: String,
    #[serde(default)]
    next_fire_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_fire_at: Option<DateTime<Utc>>,
}

impl LegacySkillTimer {
    fn into_timer(self, now: DateTime<Utc>) -> Result<Timer> {
        if self.kind != "skill" {
            return Err(Error::InvalidStore(format!(
                "unsupported legacy timer kind: {}",
                self.kind
            )));
        }
        Ok(Timer {
            id: Uuid::now_v7().to_string(),
            target: TimerTargetRef::Skill { skill: self.skill },
            enabled: self.enabled,
            schedule: Schedule::cron(&self.expr)?,
            next_fire_at: self.next_fire_at,
            last_fire_at: self.last_fire_at,
            created_at: now,
            updated_at: now,
        })
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{Duration, Utc};

    use super::*;
    use crate::timer::{Schedule, TimerTargetRef};

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "mandeven-timer-store-test-{}",
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample_timer() -> Timer {
        let now = Utc::now();
        Timer {
            id: uuid::Uuid::now_v7().to_string(),
            target: TimerTargetRef::Task {
                project: "project-a".to_string(),
                task_id: uuid::Uuid::now_v7().to_string(),
            },
            enabled: true,
            schedule: Schedule::every(Duration::minutes(15), now).unwrap(),
            next_fire_at: None,
            last_fire_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn save_then_load_round_trips_json_timers() {
        let dir = tempdir();
        let store = Store::new(&dir);
        let mut file = StoreFile::default();
        file.timers.push(sample_timer());
        store.save(&file).await.unwrap();

        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.timers.len(), 1);
        assert!(store.path().ends_with(GLOBAL_TIMER_FILENAME));
        assert!(
            tokio::fs::read_to_string(store.path())
                .await
                .unwrap()
                .starts_with("{\n")
        );

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn load_migrates_legacy_skill_timer_ids_to_uuid() {
        let dir = tempdir();
        let store = Store::new(&dir);
        tokio::fs::write(
            store.path(),
            r#"{
  "timers": [
    {
      "id": "skill:cron",
      "kind": "skill",
      "skill": "cron",
      "enabled": true,
      "expr": "0 9 * * *",
      "next_fire_at": null,
      "last_fire_at": null
    }
  ]
}
"#,
        )
        .await
        .unwrap();

        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.timers.len(), 1);
        assert!(Uuid::parse_str(&loaded.timers[0].id).is_ok());
        assert_eq!(loaded.timers[0].target.skill_name(), Some("cron"));

        let persisted = tokio::fs::read_to_string(store.path()).await.unwrap();
        assert!(persisted.contains(r#""target""#));
        assert!(!persisted.contains("skill:cron"));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
