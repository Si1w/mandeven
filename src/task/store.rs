//! On-disk persistence for project-local task state.
//!
//! The canonical store is now one Markdown document per task under
//! `<project_bucket>/tasks/`. Each file has TOML front matter for the
//! validated machine contract and a Markdown body for human-readable
//! context. Existing `tasks/tasks.json` files are still readable; the
//! next successful write materializes them as Markdown task files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::{Error, Result};
use super::{STORE_VERSION, TASK_STORE_FILENAME, TASK_SUBDIR, Task, TaskStatus};

/// On-disk compatibility shape for the legacy `tasks.json` store.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoreFile {
    /// Schema version. Values above [`STORE_VERSION`] are rejected
    /// when reading a legacy JSON store.
    pub version: u32,

    /// Legacy numeric id watermark. Unused by the Markdown store but
    /// preserved so old JSON files deserialize losslessly.
    #[serde(default)]
    pub high_watermark: u64,

    /// Tasks in insertion order.
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl StoreFile {
    /// Construct an empty store file at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: STORE_VERSION,
            high_watermark: 0,
            tasks: Vec::new(),
        }
    }
}

impl Default for StoreFile {
    fn default() -> Self {
        Self::new()
    }
}

/// Async I/O wrapper around the Markdown task directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    legacy_path: PathBuf,
}

impl Store {
    /// Construct a store rooted at `<project_bucket>/tasks/`.
    #[must_use]
    pub fn new(task_dir: &Path) -> Self {
        Self {
            dir: task_dir.to_path_buf(),
            legacy_path: task_dir.join(TASK_STORE_FILENAME),
        }
    }

    /// Path to the canonical task directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Read task Markdown files, falling back to legacy `tasks.json`
    /// when no Markdown tasks exist yet.
    ///
    /// # Errors
    ///
    /// Returns I/O, TOML, Markdown shape, JSON, or unknown-version
    /// errors.
    pub async fn load(&self) -> Result<StoreFile> {
        let mut tasks = self.load_markdown_tasks().await?;
        if !tasks.is_empty() {
            sort_tasks(&mut tasks);
            return Ok(StoreFile {
                version: STORE_VERSION,
                high_watermark: 0,
                tasks,
            });
        }
        self.load_legacy_json().await
    }

    /// Replace the Markdown task set with `file`'s tasks.
    ///
    /// Existing Markdown files for the same task id are renamed when
    /// the task title changes. Files whose front matter says
    /// `kind = "task"` but whose id is no longer present are removed.
    ///
    /// # Errors
    ///
    /// Returns I/O or TOML serialization errors.
    pub async fn save(&self, file: &StoreFile) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let existing = self.existing_markdown_tasks().await?;
        let desired_paths = self.desired_paths(&file.tasks);
        let desired_ids: BTreeSet<&str> = file.tasks.iter().map(|task| task.id.as_str()).collect();

        for task in &file.tasks {
            let Some(path) = desired_paths.get(&task.id) else {
                continue;
            };
            write_task(path, task).await?;
        }

        for existing_task in existing {
            let Some(existing_path) = markdown_path_from_task(&self.dir, &existing_task) else {
                continue;
            };
            let desired_path = desired_paths.get(&existing_task.id);
            if !desired_ids.contains(existing_task.id.as_str())
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

    async fn load_markdown_tasks(&self) -> Result<Vec<Task>> {
        self.existing_markdown_tasks().await
    }

    async fn existing_markdown_tasks(&self) -> Result<Vec<Task>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::Io(err)),
        };
        let mut tasks = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            if let Some(task) = read_task(&path).await? {
                tasks.push(task);
            }
        }
        sort_tasks(&mut tasks);
        Ok(tasks)
    }

    async fn load_legacy_json(&self) -> Result<StoreFile> {
        let bytes = match tokio::fs::read(&self.legacy_path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoreFile::new());
            }
            Err(err) => return Err(Error::Io(err)),
        };
        let parsed: StoreFile = serde_json::from_slice(&bytes)?;
        if parsed.version > STORE_VERSION {
            return Err(Error::InvalidStore(format!(
                "store version {} is newer than this build supports ({STORE_VERSION})",
                parsed.version
            )));
        }
        Ok(parsed)
    }

    fn desired_paths(&self, tasks: &[Task]) -> BTreeMap<String, PathBuf> {
        let mut paths = BTreeMap::new();
        let mut used = BTreeSet::new();
        for task in tasks {
            let base = slugify(&task.subject);
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
            paths.insert(task.id.clone(), self.dir.join(filename));
        }
        paths
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TaskFrontMatter {
    id: String,
    kind: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_form: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(default)]
    status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    blocks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TaskFrontMatter {
    fn from_task(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            kind: "task".to_string(),
            enabled: true,
            active_form: task.active_form.clone(),
            owner: task.owner.clone(),
            status: task.status,
            blocks: task.blocks.clone(),
            blocked_by: task.blocked_by.clone(),
            metadata: task.metadata.clone(),
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }

    fn into_task(self, path: &Path, subject: String, description: String) -> Task {
        Task {
            id: self.id,
            path: Some(display_path(path)),
            subject,
            description,
            active_form: self.active_form,
            owner: self.owner,
            status: self.status,
            blocks: self.blocks,
            blocked_by: self.blocked_by,
            metadata: self.metadata,
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

async fn read_task(path: &Path) -> Result<Option<Task>> {
    let content = tokio::fs::read_to_string(path).await?;
    let Some((format, front_matter, body)) = split_front_matter(&content) else {
        return Ok(None);
    };
    let front_matter = match format {
        FrontMatterFormat::Yaml => TaskFrontMatter::from_yaml(front_matter)?,
        FrontMatterFormat::Toml => toml::from_str(front_matter)?,
    };
    if front_matter.kind != "task" {
        return Ok(None);
    }
    let (subject, description) = parse_body(body)?;
    Ok(Some(front_matter.into_task(path, subject, description)))
}

async fn write_task(path: &Path, task: &Task) -> Result<()> {
    let content = render_task(task)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("task.md");
    let tmp_path = path.with_file_name(format!(".{filename}.tmp"));
    tokio::fs::write(&tmp_path, content).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

fn render_task(task: &Task) -> Result<String> {
    let mut front_matter = TaskFrontMatter::from_task(task).to_yaml()?;
    if !front_matter.ends_with('\n') {
        front_matter.push('\n');
    }
    Ok(format!(
        "---\n{front_matter}---\n\n# {}\n\n{}\n",
        task.subject.trim(),
        task.description.trim_end()
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

fn parse_body(body: &str) -> Result<(String, String)> {
    let body = body.trim_start();
    let Some((heading, rest)) = body.split_once('\n') else {
        return Err(Error::InvalidStore(
            "task Markdown body must start with a level-1 heading".to_string(),
        ));
    };
    let Some(subject) = heading.strip_prefix("# ") else {
        return Err(Error::InvalidStore(
            "task Markdown body must start with a level-1 heading".to_string(),
        ));
    };
    let subject = subject.trim().to_string();
    if subject.is_empty() {
        return Err(Error::InvalidStore(
            "task Markdown heading must not be empty".to_string(),
        ));
    }
    let description = rest.trim_start_matches('\n').trim_end().to_string();
    if description.is_empty() {
        return Err(Error::InvalidStore(
            "task Markdown body must contain a description".to_string(),
        ));
    }
    Ok((subject, description))
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

fn markdown_path_from_task(dir: &Path, task: &Task) -> Option<PathBuf> {
    let path = task.path.as_deref()?;
    let filename = path.rsplit('/').next()?;
    Some(dir.join(filename))
}

fn display_path(path: &Path) -> String {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("task.md");
    format!("{TASK_SUBDIR}/{filename}")
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
        "task".to_string()
    } else {
        out
    }
}

fn sort_tasks(tasks: &mut [Task]) {
    tasks.sort_by(|left, right| {
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
    use super::*;

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mandeven-task-store-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample_task(subject: &str) -> Task {
        let now = Utc::now();
        Task {
            id: uuid::Uuid::now_v7().to_string(),
            path: None,
            subject: subject.to_string(),
            description: format!("Do {subject}"),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn save_then_load_round_trips_markdown_tasks() {
        let dir = tempdir();
        let store = Store::new(&dir);
        let mut file = StoreFile::new();
        file.tasks.push(sample_task("Daily paper progress"));
        store.save(&file).await.unwrap();

        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(loaded.tasks[0].subject, "Daily paper progress");
        assert_eq!(
            loaded.tasks[0].path.as_deref(),
            Some("tasks/daily-paper-progress.md")
        );
        let raw = tokio::fs::read_to_string(dir.join("daily-paper-progress.md"))
            .await
            .unwrap();
        assert!(raw.starts_with("---\n"));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn duplicate_titles_get_readable_suffixes() {
        let dir = tempdir();
        let store = Store::new(&dir);
        let mut file = StoreFile::new();
        file.tasks.push(sample_task("Check build"));
        file.tasks.push(sample_task("Check build"));
        store.save(&file).await.unwrap();

        assert!(dir.join("check-build.md").exists());
        assert!(dir.join("check-build-2.md").exists());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
