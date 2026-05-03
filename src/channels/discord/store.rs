//! Persistence for the Discord adapter's allowlist.
//!
//! Lives at `<data_dir>/channels/discord/allowlist.json`. Runtime-mutable
//! state goes into a JSON sidecar so users can change it via slash
//! commands without editing `mandeven.toml`.
//!
//! Writes are atomic: payload is staged in a `<file>.tmp` sibling
//! and `rename`d into place, so a crash mid-write never leaves the
//! file half-written.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

/// Shared subdirectory for external-channel runtime state.
pub const CHANNELS_SUBDIR: &str = "channels";

/// Subdirectory of [`CHANNELS_SUBDIR`] holding Discord-specific
/// runtime state.
pub const DISCORD_SUBDIR: &str = "discord";

/// Filename inside [`DISCORD_SUBDIR`] holding the user-id allowlist.
pub const ALLOWLIST_FILENAME: &str = "allowlist.json";

#[derive(Debug, Default, Deserialize, Serialize)]
struct AllowlistFile {
    /// User ids serialized as numbers. Sorted on write so diffs in
    /// version-controlled installs stay stable across mutations.
    user_ids: Vec<u64>,
}

/// Resolve the on-disk path of the Discord allowlist sidecar.
#[must_use]
pub fn allowlist_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(CHANNELS_SUBDIR)
        .join(DISCORD_SUBDIR)
        .join(ALLOWLIST_FILENAME)
}

/// Previous v1 location. Kept only for one-way startup migration.
#[must_use]
pub fn legacy_allowlist_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DISCORD_SUBDIR).join(ALLOWLIST_FILENAME)
}

/// Load from the canonical path, falling back to the legacy path when
/// needed and copying legacy data forward.
///
/// # Errors
///
/// Returns the underlying I/O error when probing for either path
/// fails or the legacy file cannot be migrated to the canonical
/// location.
pub async fn load_or_migrate(data_dir: &Path) -> io::Result<(PathBuf, HashSet<u64>)> {
    let path = allowlist_path(data_dir);
    if fs::try_exists(&path).await? {
        return load(&path).await.map(|ids| (path, ids));
    }

    let legacy_path = legacy_allowlist_path(data_dir);
    if fs::try_exists(&legacy_path).await? {
        let ids = load(&legacy_path).await?;
        save(&path, &ids).await?;
        return Ok((path, ids));
    }
    Ok((path, HashSet::new()))
}

/// Load the allow list from disk.
///
/// A missing file yields an empty set — first launch, or a fresh
/// install before any `/discord allow` command. The agent stays in
/// the conservative deny-all state until ids are added.
///
/// # Errors
///
/// - [`io::Error`] when the file exists but cannot be read.
/// - [`io::Error`] of kind [`io::ErrorKind::InvalidData`] when the
///   contents do not parse as the expected JSON schema.
pub async fn load(path: &Path) -> io::Result<HashSet<u64>> {
    match fs::read_to_string(path).await {
        Ok(text) => {
            let parsed: AllowlistFile = serde_json::from_str(&text)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            Ok(parsed.user_ids.into_iter().collect())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(err) => Err(err),
    }
}

/// Atomically replace the allowlist file with `ids`.
///
/// Creates parent directories on first save. Writes pretty JSON so
/// hand-editing or `git diff`-ing the file is readable.
///
/// # Errors
///
/// Returns [`io::Error`] when temp-write, rename, or directory
/// creation fails. JSON encoding failure surfaces as
/// [`io::ErrorKind::InvalidData`].
pub async fn save<S: BuildHasher>(path: &Path, ids: &HashSet<u64, S>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut sorted: Vec<u64> = ids.iter().copied().collect();
    sorted.sort_unstable();
    let payload = AllowlistFile { user_ids: sorted };
    let json = serde_json::to_string_pretty(&payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ALLOWLIST_FILENAME, CHANNELS_SUBDIR, DISCORD_SUBDIR, allowlist_path, legacy_allowlist_path,
        load, load_or_migrate, save,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn tmp_dir(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mandeven-discord-{}-{}",
            label,
            uuid::Uuid::now_v7()
        ));
        p
    }

    #[test]
    fn allowlist_path_lives_under_channel_discord_subdir() {
        let base = PathBuf::from("/tmp/data");
        let p = allowlist_path(&base);
        assert!(p.ends_with(format!(
            "{CHANNELS_SUBDIR}/{DISCORD_SUBDIR}/{ALLOWLIST_FILENAME}"
        )));
    }

    #[test]
    fn legacy_allowlist_path_keeps_old_location() {
        let base = PathBuf::from("/tmp/data");
        let p = legacy_allowlist_path(&base);
        assert!(p.ends_with(format!("{DISCORD_SUBDIR}/{ALLOWLIST_FILENAME}")));
    }

    #[tokio::test]
    async fn missing_file_loads_as_empty_set() {
        let dir = tmp_dir("missing");
        let path = dir.join("allowlist.json");
        let ids = load(&path).await.expect("load missing");
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn save_then_load_round_trips_set() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join("allowlist.json");
        let mut ids = HashSet::new();
        ids.insert(1u64);
        ids.insert(2);
        ids.insert(3);
        save(&path, &ids).await.expect("save");
        let loaded = load(&path).await.expect("load");
        assert_eq!(loaded, ids);
    }

    #[tokio::test]
    async fn load_or_migrate_copies_legacy_file_forward() {
        let dir = tmp_dir("migrate");
        let legacy = legacy_allowlist_path(&dir);
        let mut ids = HashSet::new();
        ids.insert(42u64);
        save(&legacy, &ids).await.expect("save legacy");

        let (path, loaded) = load_or_migrate(&dir).await.expect("migrate");

        assert_eq!(path, allowlist_path(&dir));
        assert_eq!(loaded, ids);
        assert_eq!(load(&path).await.expect("load new"), ids);
    }
}
