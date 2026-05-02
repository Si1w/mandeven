//! Global timer state.
//!
//! User-created task timers remain backed by project Markdown for
//! now. Skill-declared timers are global because a skill lives under
//! `~/.mandeven/skills/` and should not be tied to the launch cwd.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::error::Result;
use crate::skill::SkillIndex;
use crate::timer::Schedule;

/// Global timer store filename under `~/.mandeven/`.
pub const GLOBAL_TIMER_FILENAME: &str = "timers.json";

/// Global timer JSON shape.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GlobalStoreFile {
    /// Timers in insertion order.
    #[serde(default)]
    pub timers: Vec<GlobalTimer>,
}

/// One global timer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GlobalTimer {
    /// Stable timer id. Skill timers use `skill:<name>`.
    pub id: String,
    /// Timer kind. `skill` is the only global kind today.
    pub kind: String,
    /// Referenced skill name.
    pub skill: String,
    /// `false` mutes the timer without deleting it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cron expression.
    pub expr: String,
    /// Next computed firing instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<DateTime<Utc>>,
    /// Most recent firing instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire_at: Option<DateTime<Utc>>,
}

/// Async JSON store for global timers.
#[derive(Debug)]
pub struct GlobalStore {
    path: PathBuf,
}

impl GlobalStore {
    /// Construct a store rooted at `<data_dir>/timers.json`.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(GLOBAL_TIMER_FILENAME),
        }
    }

    /// Path to the JSON store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load global timers. Missing file is an empty store.
    ///
    /// # Errors
    ///
    /// Returns timer store I/O or JSON errors when the file exists
    /// but cannot be read or decoded.
    pub async fn load(&self) -> Result<GlobalStoreFile> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(GlobalStoreFile::default())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Save global timers.
    ///
    /// # Errors
    ///
    /// Returns timer store I/O or JSON errors when the parent
    /// directory cannot be created or the file cannot be encoded or
    /// written.
    pub async fn save(&self, file: &GlobalStoreFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let raw = serde_json::to_string_pretty(file)?;
        tokio::fs::write(&self.path, format!("{raw}\n")).await?;
        Ok(())
    }
}

/// Sync skill-declared timers into the global timer store.
///
/// The declaration is intentionally tiny: `timers: "0 9 * * *"`.
/// Timer ids are derived as `skill:<skill-name>`, so discovery can
/// upsert idempotently without sidecar files.
///
/// # Errors
///
/// Returns timer store I/O or JSON errors when the global timer file
/// cannot be loaded or saved.
pub async fn sync_skill_timers(data_dir: &Path, skills: &SkillIndex) -> Result<()> {
    let store = GlobalStore::new(data_dir);
    let mut file = store.load().await?;
    let now = Utc::now();
    let mut desired = Vec::new();

    for skill in skills.skills() {
        let Some(expr) = skill.frontmatter.timers.as_deref() else {
            continue;
        };
        let expr = expr.trim();
        if expr.is_empty() {
            continue;
        }
        let schedule = match Schedule::cron(expr) {
            Ok(schedule) => schedule,
            Err(err) => {
                eprintln!(
                    "[timer] skipped invalid timer on skill /{}: {err}",
                    skill.frontmatter.name
                );
                continue;
            }
        };
        desired.push((
            format!("skill:{}", skill.frontmatter.name),
            skill.frontmatter.name.clone(),
            expr.to_string(),
            schedule.next_after(now),
        ));
    }

    file.timers.retain(|timer| {
        timer.kind != "skill" || desired.iter().any(|(id, _, _, _)| id == &timer.id)
    });

    let mut changed = false;
    for (id, skill, expr, next_fire_at) in desired {
        if let Some(existing) = file.timers.iter_mut().find(|timer| timer.id == id) {
            if existing.skill != skill {
                existing.skill.clone_from(&skill);
                changed = true;
            }
            if existing.expr != expr {
                existing.expr.clone_from(&expr);
                existing.next_fire_at = next_fire_at;
                changed = true;
            }
            continue;
        }
        file.timers.push(GlobalTimer {
            id,
            kind: "skill".to_string(),
            skill,
            enabled: true,
            expr,
            next_fire_at,
            last_fire_at: None,
        });
        changed = true;
    }

    if changed {
        store.save(&file).await?;
    }
    Ok(())
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::skill::{Skill, SkillFrontmatter, SkillIndex};

    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-global-timer-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn skill(name: &str, timers: Option<&str>) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: name.to_string(),
                description: "desc".to_string(),
                allowed_tools: Vec::new(),
                user_invocable: true,
                timers: timers.map(ToString::to_string),
                fork: false,
            },
            body: String::new(),
            source_path: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
        }
    }

    #[tokio::test]
    async fn sync_skill_timers_upserts_global_json() {
        let dir = tempdir();
        let skills = SkillIndex::from_skills(vec![skill("cron", Some("0 9 * * *"))]);
        sync_skill_timers(&dir, &skills).await.unwrap();

        let file = GlobalStore::new(&dir).load().await.unwrap();
        assert_eq!(file.timers.len(), 1);
        assert_eq!(file.timers[0].id, "skill:cron");
        assert_eq!(file.timers[0].expr, "0 9 * * *");

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
