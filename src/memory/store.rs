//! On-disk persistence for memory records and the derived user profile.
//!
//! Memory records are Markdown files under the memory directory with
//! YAML front matter for the validated machine contract and Markdown
//! bodies for human-readable detail. Legacy `memories.json` and
//! `profile.json` files are still readable; the next write
//! materializes Markdown.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::error::{Error, Result};
use super::{
    Memory, MemoryKind, MemoryScope, MemorySource, MemoryStatus, ProfileFile, STORE_VERSION,
    UserProfile,
};

/// On-disk compatibility shape for legacy `memories.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoreFile {
    /// Schema version. Values above [`STORE_VERSION`] are rejected.
    pub version: u32,

    /// Memory records in insertion order.
    #[serde(default)]
    pub memories: Vec<Memory>,
}

impl StoreFile {
    /// Construct an empty store file at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: STORE_VERSION,
            memories: Vec::new(),
        }
    }
}

impl Default for StoreFile {
    fn default() -> Self {
        Self::new()
    }
}

/// Async I/O wrapper around one Markdown memory directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    legacy_path: PathBuf,
}

impl Store {
    /// Construct a store rooted at `<dir>/`.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            legacy_path: dir.join(super::MEMORY_STORE_FILENAME),
        }
    }

    /// Path to the canonical memory directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Read memory Markdown files, falling back to legacy JSON when
    /// no Markdown memories exist yet.
    ///
    /// # Errors
    ///
    /// Returns I/O, YAML, Markdown shape, JSON, or unknown-version
    /// errors.
    pub async fn load(&self) -> Result<StoreFile> {
        let mut memories = self.existing_markdown_memories().await?;
        if !memories.is_empty() {
            sort_memories(&mut memories);
            return Ok(StoreFile {
                version: STORE_VERSION,
                memories,
            });
        }
        self.load_legacy_json().await
    }

    /// Replace the Markdown memory set with `file`'s memories.
    ///
    /// Existing Markdown files for the same memory id are renamed
    /// when the title changes. Files whose front matter says
    /// `kind = "memory"` but whose id is no longer present are
    /// removed.
    ///
    /// # Errors
    ///
    /// Returns I/O or YAML serialization errors.
    pub async fn save(&self, file: &StoreFile) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let existing = self.existing_markdown_memory_entries().await?;
        let desired_paths = self.desired_paths(&file.memories);
        let desired_ids: BTreeSet<&str> = file
            .memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect();

        for memory in &file.memories {
            let Some(path) = desired_paths.get(&memory.id) else {
                continue;
            };
            write_memory(path, memory).await?;
        }

        for (existing_memory, existing_path) in existing {
            let desired_path = desired_paths.get(&existing_memory.id);
            if !desired_ids.contains(existing_memory.id.as_str())
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

    async fn existing_markdown_memories(&self) -> Result<Vec<Memory>> {
        Ok(self
            .existing_markdown_memory_entries()
            .await?
            .into_iter()
            .map(|(memory, _path)| memory)
            .collect())
    }

    async fn existing_markdown_memory_entries(&self) -> Result<Vec<(Memory, PathBuf)>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::Io(err)),
        };
        let mut memories = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            if let Some(memory) = read_memory(&path).await? {
                memories.push((memory, path));
            }
        }
        memories.sort_by(|(left, _), (right, _)| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(memories)
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

    fn desired_paths(&self, memories: &[Memory]) -> BTreeMap<String, PathBuf> {
        let mut paths = BTreeMap::new();
        let mut used = BTreeSet::new();
        for memory in memories {
            let base = slugify(&memory.title);
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
            paths.insert(memory.id.clone(), self.dir.join(filename));
        }
        paths
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct MemoryFrontMatter {
    id: String,
    kind: String,
    scope: MemoryScope,
    memory_kind: MemoryKind,
    title: String,
    summary: String,
    #[serde(default)]
    tags: Vec<String>,
    status: MemoryStatus,
    source: MemorySource,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_verified_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review_after: Option<DateTime<Utc>>,
}

impl MemoryFrontMatter {
    fn from_memory(memory: &Memory) -> Self {
        Self {
            id: memory.id.clone(),
            kind: "memory".to_string(),
            scope: memory.scope,
            memory_kind: memory.kind,
            title: memory.title.clone(),
            summary: memory.summary.clone(),
            tags: memory.tags.clone(),
            status: memory.status,
            source: memory.source.clone(),
            created_at: memory.created_at,
            updated_at: memory.updated_at,
            last_used_at: memory.last_used_at,
            last_verified_at: memory.last_verified_at,
            review_after: memory.review_after,
        }
    }

    fn into_memory(self, body: String) -> Memory {
        Memory {
            id: self.id,
            scope: self.scope,
            kind: self.memory_kind,
            title: self.title,
            summary: self.summary,
            body,
            tags: self.tags,
            status: self.status,
            source: self.source,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_used_at: self.last_used_at,
            last_verified_at: self.last_verified_at,
            review_after: self.review_after,
        }
    }
}

async fn read_memory(path: &Path) -> Result<Option<Memory>> {
    let content = tokio::fs::read_to_string(path).await?;
    let Some((front_matter, body)) = split_yaml_front_matter(&content) else {
        return Ok(None);
    };
    let raw: serde_yaml::Value = serde_yaml::from_str(front_matter)?;
    if raw.get("kind").and_then(serde_yaml::Value::as_str) != Some("memory") {
        return Ok(None);
    }
    let front_matter: MemoryFrontMatter = serde_yaml::from_str(front_matter)?;
    let body = parse_memory_body(body)?;
    Ok(Some(front_matter.into_memory(body)))
}

async fn write_memory(path: &Path, memory: &Memory) -> Result<()> {
    let content = render_memory(memory)?;
    write_atomic(path, content).await
}

fn render_memory(memory: &Memory) -> Result<String> {
    let mut front_matter = serialize_yaml_front_matter(&MemoryFrontMatter::from_memory(memory))?;
    if !front_matter.ends_with('\n') {
        front_matter.push('\n');
    }
    Ok(format!(
        "---\n{front_matter}---\n\n# {}\n\n{}\n",
        memory.title.trim(),
        memory.body.trim_end()
    ))
}

fn parse_memory_body(body: &str) -> Result<String> {
    let body = body.trim_start();
    let Some((heading, rest)) = body.split_once('\n') else {
        return Err(Error::InvalidStore(
            "memory Markdown body must start with a level-1 heading".to_string(),
        ));
    };
    if !heading.starts_with("# ") {
        return Err(Error::InvalidStore(
            "memory Markdown body must start with a level-1 heading".to_string(),
        ));
    }
    let body = rest.trim_start_matches('\n').trim_end().to_string();
    if body.is_empty() {
        return Err(Error::InvalidStore(
            "memory Markdown body must contain body text".to_string(),
        ));
    }
    Ok(body)
}

/// Async I/O wrapper around `profile.md`.
#[derive(Debug)]
pub struct ProfileStore {
    path: PathBuf,
    legacy_path: PathBuf,
}

impl ProfileStore {
    /// Construct a profile store rooted at `<global_memory_dir>/`.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("profile.md"),
            legacy_path: dir.join(super::PROFILE_STORE_FILENAME),
        }
    }

    /// Path to the canonical profile Markdown file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the profile file. Missing file returns `None`.
    ///
    /// # Errors
    ///
    /// Returns I/O, YAML, JSON, or unknown-version errors.
    pub async fn load(&self) -> Result<Option<ProfileFile>> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(content) => {
                let Some((front_matter, _body)) = split_yaml_front_matter(&content) else {
                    return Err(Error::InvalidStore(
                        "profile Markdown is missing YAML front matter".to_string(),
                    ));
                };
                let parsed: ProfileFrontMatter = serde_yaml::from_str(front_matter)?;
                if parsed.kind != "profile" {
                    return Err(Error::InvalidStore(
                        "profile Markdown front matter kind must be profile".to_string(),
                    ));
                }
                if parsed.version > STORE_VERSION {
                    return Err(Error::InvalidStore(format!(
                        "profile version {} is newer than this build supports ({STORE_VERSION})",
                        parsed.version
                    )));
                }
                Ok(Some(ProfileFile {
                    version: parsed.version,
                    profile: parsed.profile,
                }))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => self.load_legacy_json().await,
            Err(err) => Err(Error::Io(err)),
        }
    }

    /// Atomically replace the profile Markdown file.
    ///
    /// # Errors
    ///
    /// Returns I/O or YAML serialization errors.
    pub async fn save(&self, file: &ProfileFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let content = render_profile(file)?;
        write_atomic(&self.path, content).await
    }

    async fn load_legacy_json(&self) -> Result<Option<ProfileFile>> {
        let bytes = match tokio::fs::read(&self.legacy_path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        let parsed: ProfileFile = serde_json::from_slice(&bytes)?;
        if parsed.version > STORE_VERSION {
            return Err(Error::InvalidStore(format!(
                "profile version {} is newer than this build supports ({STORE_VERSION})",
                parsed.version
            )));
        }
        Ok(Some(parsed))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProfileFrontMatter {
    kind: String,
    version: u32,
    profile: UserProfile,
}

fn render_profile(file: &ProfileFile) -> Result<String> {
    let front = ProfileFrontMatter {
        kind: "profile".to_string(),
        version: file.version,
        profile: file.profile.clone(),
    };
    let mut front_matter = serialize_yaml_front_matter(&front)?;
    if !front_matter.ends_with('\n') {
        front_matter.push('\n');
    }
    Ok(format!(
        "---\n{front_matter}---\n\n{}",
        profile_body(&file.profile)
    ))
}

fn profile_body(profile: &UserProfile) -> String {
    let mut out = String::from("# Memory Profile\n\n");
    if profile.summary.trim().is_empty() {
        out.push_str("_No durable profile yet._\n");
    } else {
        let _ = writeln!(out, "{}", profile.summary.trim());
    }
    write_list(
        &mut out,
        "Communication Style",
        &profile.communication_style,
    );
    write_list(
        &mut out,
        "Working Preferences",
        &profile.working_preferences,
    );
    write_list(&mut out, "Avoid", &profile.avoid);
    write_list(&mut out, "Source Memory IDs", &profile.source_memory_ids);
    out
}

fn write_list(out: &mut String, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = write!(out, "\n## {heading}\n");
    for item in items {
        let _ = writeln!(out, "- {item}");
    }
}

fn split_yaml_front_matter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let front_matter = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    Some((front_matter, body))
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

async fn write_atomic(path: &Path, content: String) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("memory.md");
    let tmp_path = path.with_file_name(format!(".{filename}.tmp"));
    tokio::fs::write(&tmp_path, content).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
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
        "memory".to_string()
    } else {
        out
    }
}

fn sort_memories(memories: &mut [Memory]) {
    memories.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
}
