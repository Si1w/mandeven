//! Skill discovery and frontmatter parsing.
//!
//! Scans `<data_dir>/skills/<name>/SKILL.md` once at boot — same
//! shape Claude Code uses for `~/.claude/skills/<name>/SKILL.md` (see
//! `agent-examples/claude-code-analysis/src/skills/loadSkillsDir.ts`).
//!
//! Frontmatter parsing is hand-rolled for the v1 minimal shape (just
//! `name` and `description`). Once the schema grows beyond flat
//! single-line strings, swap in `serde_yml` or `gray_matter` — the
//! `parse_frontmatter` private function is the only call site.

use std::fs;
use std::path::Path;

use super::error::{Error, Result};
use super::types::{Skill, SkillFrontmatter, SkillIndex};

/// Filename inside each skill directory that holds the metadata +
/// body. Matches Claude Code's `SKILL.md` convention.
pub const SKILL_FILENAME: &str = "SKILL.md";

/// Discover and parse every skill under `skills_dir`.
///
/// Missing directory ⇒ `Ok(SkillIndex::new())` — skills are an
/// optional capability, not a required one. Other I/O failures bubble
/// up as [`Error::DirRead`].
///
/// Per-skill failures (malformed frontmatter, missing fields, name
/// mismatch) are **logged to stderr but skipped** so one broken
/// SKILL.md does not block the rest of the catalog. This matches
/// Claude Code's resilience: one bad command file should not nuke
/// the whole `/<skill>` namespace.
///
/// # Errors
///
/// - [`Error::DirRead`] when the directory exists but is unreadable.
pub fn load(skills_dir: &Path) -> Result<SkillIndex> {
    let entries = match fs::read_dir(skills_dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SkillIndex::new());
        }
        Err(source) => {
            return Err(Error::DirRead {
                path: skills_dir.to_path_buf(),
                source,
            });
        }
    };

    let mut skills: Vec<Skill> = Vec::new();
    let mut entries: Vec<_> = entries.flatten().collect();
    // Stable order for the prompt section + overlay rendering.
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let dir_path = entry.path();
        if !dir_path.is_dir() {
            continue;
        }
        let dir_name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        let skill_path = dir_path.join(SKILL_FILENAME);
        if !skill_path.exists() {
            continue;
        }

        match load_one(&skill_path, &dir_name) {
            Ok(skill) => skills.push(skill),
            Err(err) => {
                eprintln!("[skill] skipped {}: {err}", skill_path.display());
            }
        }
    }

    Ok(SkillIndex::from_skills(skills))
}

/// Load one SKILL.md file and verify its frontmatter is consistent
/// with the directory name.
fn load_one(skill_path: &Path, dir_name: &str) -> Result<Skill> {
    let raw = fs::read_to_string(skill_path).map_err(|source| Error::SkillRead {
        path: skill_path.to_path_buf(),
        source,
    })?;

    let (frontmatter, body) = split_frontmatter(skill_path, &raw)?;
    let frontmatter = parse_frontmatter(skill_path, frontmatter)?;

    if frontmatter.name != dir_name {
        return Err(Error::NameMismatch {
            path: skill_path.to_path_buf(),
            declared: frontmatter.name,
            dir: dir_name.to_string(),
        });
    }

    Ok(Skill {
        frontmatter,
        body: body.trim_end().to_string(),
        source_path: skill_path.to_path_buf(),
    })
}

/// Split a SKILL.md into `(frontmatter_block, body)`.
///
/// Expected shape:
///
/// ```text
/// ---
/// name: foo
/// description: ...
/// ---
/// <markdown body>
/// ```
///
/// The opening `---` must be the very first line; the closing `---`
/// must be on its own line. Anything before the opening fence is
/// rejected — we want the file to be a SKILL.md, not a generic
/// markdown file with optional frontmatter.
fn split_frontmatter<'a>(path: &Path, raw: &'a str) -> Result<(&'a str, &'a str)> {
    let stripped = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .ok_or_else(|| Error::FrontmatterParse {
            path: path.to_path_buf(),
            reason: "file does not start with '---' fence".into(),
        })?;

    // Find the closing `---` on its own line. Try both line-ending
    // shapes — same-byte fence length only exists for the LF case.
    let (fence_offset, fence_len) = if let Some(off) = stripped.find("\n---\n") {
        (off, "\n---\n".len())
    } else if let Some(off) = stripped.find("\n---\r\n") {
        (off, "\n---\r\n".len())
    } else {
        return Err(Error::FrontmatterParse {
            path: path.to_path_buf(),
            reason: "missing closing '---' fence".into(),
        });
    };

    let frontmatter = &stripped[..fence_offset];
    let body_start = fence_offset + fence_len;
    let body = stripped.get(body_start..).unwrap_or("");

    Ok((frontmatter, body))
}

/// Parse the frontmatter block into a [`SkillFrontmatter`].
///
/// Hand-rolled for v1's two-field schema — handles `key: value`,
/// optional surrounding quotes (single or double), and ignores blank
/// lines + `# comment` lines. Multi-line values, nested objects, and
/// arrays are all unsupported and would throw a parse error here;
/// when we need them, swap in `serde_yml`.
fn parse_frontmatter(path: &Path, block: &str) -> Result<SkillFrontmatter> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;

    for raw_line in block.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(Error::FrontmatterParse {
                path: path.to_path_buf(),
                reason: format!("line missing ':' separator: {raw_line:?}"),
            });
        };
        let key = key.trim();
        let value = strip_quotes(value.trim());

        match key {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            // Unknown keys are tolerated — future fields can land in
            // SKILL.md before they're parsed here. Logging would be
            // noisy at this layer; loader.rs's eprintln on outright
            // failure is enough.
            _ => {}
        }
    }

    let name = name.ok_or(Error::MissingField {
        path: path.to_path_buf(),
        field: "name",
    })?;
    let description = description.ok_or(Error::MissingField {
        path: path.to_path_buf(),
        field: "description",
    })?;

    Ok(SkillFrontmatter { name, description })
}

/// Strip a single layer of matching `'…'` or `"…"` quotes if present.
/// Otherwise return the input unchanged.
fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("mandeven-skill-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_skill(root: &Path, name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn load_returns_empty_when_dir_missing() {
        let idx = load(&tempdir().join("nonexistent")).unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn load_skips_malformed_skill_but_keeps_others() {
        let dir = tempdir();
        write_skill(
            &dir,
            "good",
            "name: good\ndescription: works",
            "# Body\nstuff",
        );
        // Malformed: no closing fence.
        fs::create_dir_all(dir.join("broken")).unwrap();
        fs::write(
            dir.join("broken/SKILL.md"),
            "---\nname: broken\ndescription: bad",
        )
        .unwrap();

        let idx = load(&dir).unwrap();
        assert_eq!(idx.len(), 1);
        assert!(idx.get("good").is_some());
        assert!(idx.get("broken").is_none());
    }

    #[test]
    fn load_rejects_name_mismatch_with_directory() {
        let dir = tempdir();
        write_skill(&dir, "actual-dir", "name: declared\ndescription: x", "");
        let idx = load(&dir).unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn parse_frontmatter_handles_quoted_description() {
        let path = std::path::PathBuf::from("/tmp/x");
        let fm = parse_frontmatter(
            &path,
            "name: foo\ndescription: \"says \\\"hi\\\" politely\"",
        )
        .unwrap();
        assert_eq!(fm.name, "foo");
        // Outer quotes stripped; inner escapes left literal — v1 is
        // not a YAML lib, just a flat key:value reader.
        assert_eq!(fm.description, "says \\\"hi\\\" politely");
    }

    #[test]
    fn parse_frontmatter_ignores_blank_and_comment_lines() {
        let path = std::path::PathBuf::from("/tmp/x");
        let fm = parse_frontmatter(&path, "# comment\nname: bar\n\ndescription: ok\n").unwrap();
        assert_eq!(fm.name, "bar");
        assert_eq!(fm.description, "ok");
    }

    #[test]
    fn split_frontmatter_extracts_block_and_body() {
        let path = std::path::PathBuf::from("/tmp/x");
        let raw = "---\nname: foo\ndescription: bar\n---\n# Body\nhello";
        let (fm, body) = split_frontmatter(&path, raw).unwrap();
        assert_eq!(fm, "name: foo\ndescription: bar");
        assert_eq!(body, "# Body\nhello");
    }

    #[test]
    fn split_frontmatter_rejects_missing_opening_fence() {
        let path = std::path::PathBuf::from("/tmp/x");
        let err = split_frontmatter(&path, "name: foo\n---\nbody").unwrap_err();
        assert!(matches!(err, Error::FrontmatterParse { .. }));
    }

    #[test]
    fn load_orders_skills_by_directory_name() {
        let dir = tempdir();
        write_skill(&dir, "zulu", "name: zulu\ndescription: z", "");
        write_skill(&dir, "alpha", "name: alpha\ndescription: a", "");
        let idx = load(&dir).unwrap();
        let names: Vec<&str> = idx.entries().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["alpha", "zulu"]);
    }
}
