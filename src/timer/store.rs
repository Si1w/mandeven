//! On-disk persistence for project-local timer state.
//!
//! Timers are Markdown files under `<project_bucket>/timers/` with
//! TOML front matter for validated machine state. The Markdown body is
//! intentionally small: the heading is the user-visible title.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::error::{Error, Result};
use super::{TIMER_SUBDIR, Timer};
use crate::cron::Schedule;

/// In-memory timer store shape.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StoreFile {
    /// Timers in insertion order.
    #[serde(default)]
    pub timers: Vec<Timer>,
}

/// Async I/O wrapper around the Markdown timer directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Construct a store rooted at `<project_bucket>/timers/`.
    #[must_use]
    pub fn new(timer_dir: &Path) -> Self {
        Self {
            dir: timer_dir.to_path_buf(),
        }
    }

    /// Path to the canonical timer directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Read timer Markdown files. Missing directory returns an empty
    /// store.
    ///
    /// # Errors
    ///
    /// Returns I/O, TOML, or Markdown shape errors.
    pub async fn load(&self) -> Result<StoreFile> {
        let mut timers = self.existing_markdown_timers().await?;
        sort_timers(&mut timers);
        Ok(StoreFile { timers })
    }

    /// Replace the Markdown timer set with `file`'s timers.
    ///
    /// Existing Markdown files for the same timer id are renamed when
    /// the title changes. Files whose front matter says
    /// `kind = "timer"` but whose id is no longer present are removed.
    ///
    /// # Errors
    ///
    /// Returns I/O or TOML serialization errors.
    pub async fn save(&self, file: &StoreFile) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let existing = self.existing_markdown_timers().await?;
        let desired_paths = self.desired_paths(&file.timers);
        let desired_ids: BTreeSet<&str> =
            file.timers.iter().map(|timer| timer.id.as_str()).collect();

        for timer in &file.timers {
            let Some(path) = desired_paths.get(&timer.id) else {
                continue;
            };
            write_timer(path, timer).await?;
        }

        for existing_timer in existing {
            let Some(existing_path) = markdown_path_from_timer(&self.dir, &existing_timer) else {
                continue;
            };
            let desired_path = desired_paths.get(&existing_timer.id);
            if !desired_ids.contains(existing_timer.id.as_str())
                || desired_path != Some(&existing_path)
            {
                match tokio::fs::remove_file(&existing_path).await {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(Error::Io(err)),
                }
            }
        }
        Ok(())
    }

    async fn existing_markdown_timers(&self) -> Result<Vec<Timer>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::Io(err)),
        };
        let mut timers = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            if let Some(timer) = read_timer(&path).await? {
                timers.push(timer);
            }
        }
        sort_timers(&mut timers);
        Ok(timers)
    }

    fn desired_paths(&self, timers: &[Timer]) -> BTreeMap<String, PathBuf> {
        let mut paths = BTreeMap::new();
        let mut used = BTreeSet::new();
        for timer in timers {
            let base = slugify(&timer.title);
            let mut suffix = 1_u64;
            let filename = loop {
                let candidate = if suffix == 1 {
                    format!("{base}.md")
                } else {
                    format!("{base}-{suffix}.md")
                };
                if used.insert(candidate.clone()) {
                    break candidate;
                }
                suffix = suffix.saturating_add(1);
            };
            paths.insert(timer.id.clone(), self.dir.join(filename));
        }
        paths
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TimerFrontMatter {
    id: String,
    kind: String,
    #[serde(default = "default_true")]
    enabled: bool,
    task_id: String,
    schedule: Schedule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_fire_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_fire_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TimerFrontMatter {
    fn from_timer(timer: &Timer) -> Self {
        Self {
            id: timer.id.clone(),
            kind: "timer".to_string(),
            enabled: timer.enabled,
            task_id: timer.task_id.clone(),
            schedule: timer.schedule.clone(),
            next_fire_at: timer.next_fire_at,
            last_fire_at: timer.last_fire_at,
            created_at: timer.created_at,
            updated_at: timer.updated_at,
        }
    }

    fn into_timer(self, path: &Path, title: String) -> Timer {
        Timer {
            id: self.id,
            path: Some(display_path(path)),
            title,
            task_id: self.task_id,
            enabled: self.enabled,
            schedule: self.schedule,
            next_fire_at: self.next_fire_at,
            last_fire_at: self.last_fire_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    fn from_yaml(raw: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(raw)?)
    }

    fn to_yaml(&self) -> Result<String> {
        serialize_yaml_front_matter(self)
    }
}

async fn read_timer(path: &Path) -> Result<Option<Timer>> {
    let content = tokio::fs::read_to_string(path).await?;
    let Some((format, front_matter, body)) = split_front_matter(&content) else {
        return Ok(None);
    };
    let front_matter = match format {
        FrontMatterFormat::Yaml => TimerFrontMatter::from_yaml(front_matter)?,
        FrontMatterFormat::Toml => toml::from_str(front_matter)?,
    };
    if front_matter.kind != "timer" {
        return Ok(None);
    }
    let title = parse_title(body)?;
    Ok(Some(front_matter.into_timer(path, title)))
}

async fn write_timer(path: &Path, timer: &Timer) -> Result<()> {
    let content = render_timer(timer)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("timer.md");
    let tmp_path = path.with_file_name(format!(".{filename}.tmp"));
    tokio::fs::write(&tmp_path, content).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

fn render_timer(timer: &Timer) -> Result<String> {
    let mut front_matter = TimerFrontMatter::from_timer(timer).to_yaml()?;
    if !front_matter.ends_with('\n') {
        front_matter.push('\n');
    }
    Ok(format!(
        "---\n{front_matter}---\n\n# {}\n",
        timer.title.trim()
    ))
}

#[derive(Clone, Copy)]
enum FrontMatterFormat {
    Yaml,
    Toml,
}

fn split_front_matter(content: &str) -> Option<(FrontMatterFormat, &str, &str)> {
    if let Some(parts) = split_front_matter_with(content, "---\n", "\n---\n") {
        return Some((FrontMatterFormat::Yaml, parts.0, parts.1));
    }
    split_front_matter_with(content, "+++\n", "\n+++\n")
        .map(|(front_matter, body)| (FrontMatterFormat::Toml, front_matter, body))
}

fn split_front_matter_with<'a>(
    content: &'a str,
    opening: &str,
    delimiter: &str,
) -> Option<(&'a str, &'a str)> {
    let rest = content.strip_prefix(opening)?;
    let end = rest.find(delimiter)?;
    let front_matter = &rest[..end];
    let body = &rest[end + delimiter.len()..];
    Some((front_matter, body))
}

fn parse_title(body: &str) -> Result<String> {
    let body = body.trim_start();
    let heading = body.lines().next().ok_or_else(|| {
        Error::InvalidStore("timer Markdown body must start with a level-1 heading".to_string())
    })?;
    let Some(title) = heading.strip_prefix("# ") else {
        return Err(Error::InvalidStore(
            "timer Markdown body must start with a level-1 heading".to_string(),
        ));
    };
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err(Error::InvalidStore(
            "timer Markdown heading must not be empty".to_string(),
        ));
    }
    Ok(title)
}

fn serialize_yaml_front_matter<T: Serialize>(value: &T) -> Result<String> {
    let mut yaml = serde_yaml::to_string(value)?;
    if let Some(stripped) = yaml.strip_prefix("---\n") {
        yaml = stripped.to_string();
    }
    if yaml.ends_with("...\n") {
        yaml.truncate(yaml.len() - "...\n".len());
    }
    Ok(yaml)
}

fn markdown_path_from_timer(dir: &Path, timer: &Timer) -> Option<PathBuf> {
    let path = timer.path.as_deref()?;
    let filename = path.rsplit('/').next()?;
    Some(dir.join(filename))
}

fn display_path(path: &Path) -> String {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("timer.md");
    format!("{TIMER_SUBDIR}/{filename}")
}

fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;
    for ch in title.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            previous_dash = false;
        } else if !previous_dash && !out.is_empty() {
            out.push('-');
            previous_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "timer".to_string()
    } else {
        out
    }
}

fn sort_timers(timers: &mut [Timer]) {
    timers.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "mandeven-timer-store-test-{}",
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample_timer(title: &str) -> Timer {
        let now = Utc::now();
        Timer {
            id: uuid::Uuid::now_v7().to_string(),
            path: None,
            title: title.to_string(),
            task_id: uuid::Uuid::now_v7().to_string(),
            enabled: true,
            schedule: Schedule::every(Duration::minutes(15), now).unwrap(),
            next_fire_at: None,
            last_fire_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn save_then_load_round_trips_markdown_timers() {
        let dir = tempdir();
        let store = Store::new(&dir);
        let mut file = StoreFile::default();
        file.timers.push(sample_timer("Daily paper progress"));
        store.save(&file).await.unwrap();

        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.timers.len(), 1);
        assert_eq!(loaded.timers[0].title, "Daily paper progress");
        assert_eq!(
            loaded.timers[0].path.as_deref(),
            Some("timers/daily-paper-progress.md")
        );
        let raw = tokio::fs::read_to_string(dir.join("daily-paper-progress.md"))
            .await
            .unwrap();
        assert!(raw.starts_with("---\n"));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
