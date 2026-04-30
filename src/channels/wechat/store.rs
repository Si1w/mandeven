//! Persistence for the `WeChat` channel.
//!
//! All runtime-mutable channel state lives under
//! `<data_dir>/channels/wechat/` so future adapters can use the same
//! top-level namespace instead of scattering files in `~/.mandeven`.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::fs;

use super::api::WechatCredentials;

/// Common subdirectory for all external channel runtime state.
pub const CHANNELS_SUBDIR: &str = "channels";

/// `WeChat` channel subdirectory.
pub const WECHAT_SUBDIR: &str = "wechat";

/// Filename holding the allowed `WeChat` peer ids.
pub const ALLOWLIST_FILENAME: &str = "allowlist.json";

#[derive(Debug, Default, Deserialize, Serialize)]
struct AllowlistFile {
    user_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AccountFile {
    token: String,
    base_url: String,
    #[serde(default)]
    user_id: String,
    saved_at: String,
}

/// Resolve `<data_dir>/channels/wechat`.
#[must_use]
pub fn channel_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(CHANNELS_SUBDIR).join(WECHAT_SUBDIR)
}

/// Resolve the allowlist sidecar path.
#[must_use]
pub fn allowlist_path(data_dir: &Path) -> PathBuf {
    channel_dir(data_dir).join(ALLOWLIST_FILENAME)
}

/// Resolve the accounts directory.
#[must_use]
pub fn accounts_dir(data_dir: &Path) -> PathBuf {
    channel_dir(data_dir).join("accounts")
}

/// Resolve one account credential file.
#[must_use]
pub fn account_path(data_dir: &Path, account_id: &str) -> PathBuf {
    accounts_dir(data_dir).join(format!("{account_id}.json"))
}

fn sync_path(data_dir: &Path, account_id: &str) -> PathBuf {
    channel_dir(data_dir)
        .join("sync")
        .join(format!("{account_id}.json"))
}

fn context_tokens_path(data_dir: &Path, account_id: &str) -> PathBuf {
    channel_dir(data_dir)
        .join("context-tokens")
        .join(format!("{account_id}.json"))
}

/// Load the allow list from disk. Missing file means deny-all.
///
/// # Errors
///
/// Returns an error when the file cannot be read or parsed.
pub async fn load_allowlist(path: &Path) -> io::Result<HashSet<String>> {
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

/// Atomically save the allow list.
///
/// # Errors
///
/// Returns an error when the parent directory or JSON file cannot be written.
pub async fn save_allowlist<S: BuildHasher>(
    path: &Path,
    ids: &HashSet<String, S>,
) -> io::Result<()> {
    let mut sorted: Vec<String> = ids.iter().cloned().collect();
    sorted.sort();
    write_json(path, &AllowlistFile { user_ids: sorted }, false).await
}

/// Persist one QR-login account.
///
/// # Errors
///
/// Returns an error when credentials are incomplete or the account file cannot
/// be written.
pub async fn save_account(data_dir: &Path, creds: &WechatCredentials) -> io::Result<PathBuf> {
    if !creds.is_complete() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot save incomplete WeChat credentials",
        ));
    }
    let path = account_path(data_dir, &creds.account_id);
    let payload = AccountFile {
        token: creds.token.clone(),
        base_url: creds.base_url.clone(),
        user_id: creds.user_id.clone(),
        saved_at: chrono::Utc::now().to_rfc3339(),
    };
    write_json(&path, &payload, true).await?;
    Ok(path)
}

/// Load a specific account by id.
///
/// # Errors
///
/// Returns an error when the account file cannot be read or parsed.
pub async fn load_account(
    data_dir: &Path,
    account_id: &str,
) -> io::Result<Option<WechatCredentials>> {
    let path = account_path(data_dir, account_id);
    match fs::read_to_string(&path).await {
        Ok(text) => {
            let parsed: AccountFile = serde_json::from_str(&text)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            Ok(Some(WechatCredentials {
                account_id: account_id.to_string(),
                token: parsed.token,
                base_url: parsed.base_url,
                user_id: parsed.user_id,
            }))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Load the newest saved account, if any.
///
/// # Errors
///
/// Returns an error when the accounts directory cannot be read.
pub async fn load_latest_account(data_dir: &Path) -> io::Result<Option<WechatCredentials>> {
    let dir = accounts_dir(data_dir);
    let mut entries = match fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    let mut best: Option<(SystemTime, WechatCredentials)> = None;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(Some(creds)) = load_account(data_dir, stem).await else {
            continue;
        };
        let modified = entry
            .metadata()
            .await
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        match &best {
            Some((best_time, _)) if *best_time >= modified => {}
            _ => best = Some((modified, creds)),
        }
    }
    Ok(best.map(|(_, creds)| creds))
}

/// Delete a saved account.
///
/// # Errors
///
/// Returns an error when the account file cannot be removed.
pub async fn delete_account(data_dir: &Path, account_id: &str) -> io::Result<bool> {
    let path = account_path(data_dir, account_id);
    match fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Load the iLink long-poll cursor for an account.
///
/// # Errors
///
/// Returns an error when the cursor file cannot be read.
pub async fn load_sync_buf(data_dir: &Path, account_id: &str) -> io::Result<String> {
    let path = sync_path(data_dir, account_id);
    match fs::read_to_string(path).await {
        Ok(text) => Ok(serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("get_updates_buf")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err),
    }
}

/// Save the iLink long-poll cursor for an account.
///
/// # Errors
///
/// Returns an error when the cursor file cannot be written.
pub async fn save_sync_buf(data_dir: &Path, account_id: &str, sync_buf: &str) -> io::Result<()> {
    write_json(
        &sync_path(data_dir, account_id),
        &json!({ "get_updates_buf": sync_buf }),
        false,
    )
    .await
}

/// Load context tokens keyed by peer id.
///
/// # Errors
///
/// Returns an error when the context-token file cannot be read or parsed.
pub async fn load_context_tokens(
    data_dir: &Path,
    account_id: &str,
) -> io::Result<HashMap<String, String>> {
    let path = context_tokens_path(data_dir, account_id);
    match fs::read_to_string(path).await {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(err) => Err(err),
    }
}

/// Save context tokens keyed by peer id.
///
/// # Errors
///
/// Returns an error when the context-token file cannot be written.
pub async fn save_context_tokens<S: BuildHasher>(
    data_dir: &Path,
    account_id: &str,
    tokens: &HashMap<String, String, S>,
) -> io::Result<()> {
    write_json(&context_tokens_path(data_dir, account_id), tokens, true).await
}

async fn write_json<T: Serialize>(path: &Path, payload: &T, private: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).await?;
    fs::rename(&tmp, path).await?;
    if private {
        chmod_private(path);
    }
    Ok(())
}

fn chmod_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::{allowlist_path, load_account, save_account};
    use crate::channels::wechat::api::WechatCredentials;
    use std::path::PathBuf;

    fn tmp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mandeven-wechat-{label}-{}", uuid::Uuid::now_v7()))
    }

    #[test]
    fn allowlist_path_lives_under_channel_namespace() {
        let base = PathBuf::from("/tmp/data");
        let path = allowlist_path(&base);
        assert!(path.ends_with("channels/wechat/allowlist.json"));
    }

    #[tokio::test]
    async fn account_save_load_round_trips() {
        let dir = tmp_dir("account");
        let creds = WechatCredentials {
            account_id: "acct".into(),
            token: "token".into(),
            base_url: "https://example.com".into(),
            user_id: "user".into(),
        };
        save_account(&dir, &creds).await.expect("save");
        let loaded = load_account(&dir, "acct")
            .await
            .expect("load")
            .expect("present");
        assert_eq!(loaded.account_id, "acct");
        assert_eq!(loaded.token, "token");
        assert_eq!(loaded.base_url, "https://example.com");
        assert_eq!(loaded.user_id, "user");
    }
}
