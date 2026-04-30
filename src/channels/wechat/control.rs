//! Runtime control handle for the `WeChat` adapter.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{Mutex, watch};

use crate::channels::common::AllowList;
use crate::config::WechatConfig;

use super::api::{self, QrLoginStart, WechatCredentials};
use super::store;

const SINGLE_USER_MS0_ERROR: &str =
    "wechat MS0 supports one allowed user; deny the existing user before allowing another";

/// Snapshot rendered by `/wechat status`.
#[derive(Clone, Debug)]
pub struct WechatStatus {
    /// Whether a connection is desired.
    pub active: bool,
    /// Number of allowed `WeChat` peer ids.
    pub allowed_count: usize,
    /// Currently staged/connected account id.
    pub account_id: Option<String>,
}

/// A QR login flow after the QR code has been fetched.
pub struct WechatLogin {
    /// Scannable URL or mini-app payload returned by iLink.
    pub scan_data: String,
    /// Terminal QR rendering of [`Self::scan_data`].
    pub qr_ascii: String,
    client: Client,
    start: QrLoginStart,
}

/// Handle for allowlist, connection, credentials, and QR login.
#[derive(Clone)]
pub struct WechatControl {
    allowed: AllowList<String>,
    store_path: Arc<PathBuf>,
    data_dir: Arc<PathBuf>,
    active: Arc<watch::Sender<bool>>,
    credentials: Arc<Mutex<Option<WechatCredentials>>>,
    cfg: Arc<WechatConfig>,
}

impl WechatControl {
    /// Construct a control handle paired with a [`super::WechatChannel`].
    #[must_use]
    pub fn new(
        allowed: AllowList<String>,
        store_path: PathBuf,
        data_dir: PathBuf,
        active: Arc<watch::Sender<bool>>,
        credentials: Arc<Mutex<Option<WechatCredentials>>>,
        cfg: WechatConfig,
    ) -> Self {
        Self {
            allowed,
            store_path: Arc::new(store_path),
            data_dir: Arc::new(data_dir),
            active,
            credentials,
            cfg: Arc::new(cfg),
        }
    }

    /// Add one `WeChat` peer id to the allow list.
    ///
    /// # Errors
    ///
    /// Returns an error when the id is empty, the single-user MS0 limit would
    /// be exceeded, or the updated allowlist cannot be persisted.
    pub async fn allow(&self, user_id: String) -> io::Result<bool> {
        let user_id = user_id.trim().to_string();
        if user_id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "wechat user id must not be empty",
            ));
        }
        let current = self.allowed.snapshot();
        if current.contains(&user_id) {
            return Ok(false);
        }
        if !current.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                SINGLE_USER_MS0_ERROR,
            ));
        }
        let added = self.allowed.insert(user_id);
        if added {
            self.persist().await?;
        }
        Ok(added)
    }

    /// Remove one `WeChat` peer id from the allow list.
    ///
    /// # Errors
    ///
    /// Returns an error when the updated allowlist cannot be persisted.
    pub async fn deny(&self, user_id: &str) -> io::Result<bool> {
        let removed = self.allowed.remove(&user_id.to_string());
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    /// Sorted allowlist snapshot.
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        let mut v = self.allowed.snapshot();
        v.sort();
        v
    }

    /// Open the long-poll connection. Re-resolves credentials each time.
    ///
    /// # Errors
    ///
    /// Returns an error when credentials cannot be resolved.
    pub async fn enable(&self) -> io::Result<bool> {
        if *self.active.borrow() {
            return Ok(false);
        }
        let creds = self.resolve_credentials().await?;
        *self.credentials.lock().await = Some(creds);
        let _ = self.active.send(true);
        Ok(true)
    }

    /// Close the long-poll connection.
    #[must_use]
    pub fn disable(&self) -> bool {
        if !*self.active.borrow() {
            return false;
        }
        let _ = self.active.send(false);
        true
    }

    /// Subscribe to active-state transitions.
    #[must_use]
    pub fn subscribe_active(&self) -> watch::Receiver<bool> {
        self.active.subscribe()
    }

    /// Begin QR login and return the QR payload for display.
    ///
    /// # Errors
    ///
    /// Returns an error when iLink fails to issue a QR login payload.
    pub async fn begin_login(&self) -> io::Result<WechatLogin> {
        let client = Client::new();
        let start = api::request_qr(&client, &self.cfg.base_url).await?;
        let scan_data = start.scan_data.clone();
        let qr_ascii = api::render_qr_ascii(&scan_data);
        Ok(WechatLogin {
            scan_data,
            qr_ascii,
            client,
            start,
        })
    }

    /// Finish QR login, persist the account, and stage credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when QR confirmation fails or credentials cannot be
    /// persisted.
    pub async fn finish_login(&self, login: WechatLogin) -> io::Result<WechatCredentials> {
        let creds = api::wait_for_qr_confirmation(
            &login.client,
            &login.start,
            Duration::from_secs(self.cfg.login_timeout_secs.max(1)),
        )
        .await?;
        store::save_account(&self.data_dir, &creds).await?;
        *self.credentials.lock().await = Some(creds.clone());
        Ok(creds)
    }

    /// Delete the currently staged or latest saved account.
    ///
    /// # Errors
    ///
    /// Returns an error when saved account state cannot be read or deleted.
    pub async fn logout(&self) -> io::Result<Option<String>> {
        let _ = self.disable();
        let account_id = {
            let staged = self.credentials.lock().await;
            staged.as_ref().map(|c| c.account_id.clone())
        };

        let account_id = match account_id {
            Some(id) => id,
            None => match store::load_latest_account(&self.data_dir).await? {
                Some(creds) => creds.account_id,
                None => return Ok(None),
            },
        };
        *self.credentials.lock().await = None;
        store::delete_account(&self.data_dir, &account_id).await?;
        Ok(Some(account_id))
    }

    /// Current runtime snapshot.
    #[must_use]
    pub fn status(&self) -> WechatStatus {
        let account_id = self
            .credentials
            .try_lock()
            .ok()
            .and_then(|creds| creds.as_ref().map(|c| c.account_id.clone()));
        WechatStatus {
            active: *self.active.borrow(),
            allowed_count: self.allowed.snapshot().len(),
            account_id,
        }
    }

    async fn persist(&self) -> io::Result<()> {
        let ids: HashSet<String> = self.allowed.snapshot().into_iter().collect();
        store::save_allowlist(&self.store_path, &ids).await
    }

    async fn resolve_credentials(&self) -> io::Result<WechatCredentials> {
        let token = read_env_first(&[&self.cfg.token_env, "WEIXIN_TOKEN"]);
        let account_id = read_env_first(&[&self.cfg.account_id_env, "WEIXIN_ACCOUNT_ID"]);

        if let Some(account_id) = account_id {
            let saved = store::load_account(&self.data_dir, &account_id).await?;
            let token = token.or_else(|| saved.as_ref().map(|c| c.token.clone()));
            if let Some(token) = token {
                return Ok(WechatCredentials {
                    account_id,
                    token,
                    base_url: saved
                        .as_ref()
                        .map(|c| c.base_url.clone())
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| self.cfg.base_url.clone()),
                    user_id: saved
                        .as_ref()
                        .map(|c| c.user_id.clone())
                        .unwrap_or_default(),
                });
            }
        }

        if let Some(creds) = store::load_latest_account(&self.data_dir).await? {
            return Ok(creds);
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "${} / ${} not set and no saved WeChat account exists; run /wechat login",
                self.cfg.token_env, self.cfg.account_id_env
            ),
        ))
    }
}

fn read_env_first(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::WechatControl;
    use crate::channels::common::AllowList;
    use crate::config::WechatConfig;
    use std::sync::Arc;
    use tokio::sync::{Mutex, watch};

    fn fixture() -> WechatControl {
        let data_dir =
            std::env::temp_dir().join(format!("mandeven-wechat-control-{}", uuid::Uuid::now_v7()));
        let (active, _rx) = watch::channel(false);
        WechatControl::new(
            AllowList::with_initial(std::iter::empty::<String>()),
            data_dir.join("allowlist.json"),
            data_dir,
            Arc::new(active),
            Arc::new(Mutex::new(None)),
            WechatConfig {
                enabled: false,
                token_env: "WECHAT_TOKEN_TEST".to_string(),
                account_id_env: "WECHAT_ACCOUNT_ID_TEST".to_string(),
                base_url: "https://example.com".to_string(),
                login_timeout_secs: 1,
            },
        )
    }

    #[tokio::test]
    async fn allow_rejects_second_user_in_ms0() {
        let ctl = fixture();
        assert!(ctl.allow("wxid_1".to_string()).await.expect("allow"));
        let err = ctl
            .allow("wxid_2".to_string())
            .await
            .expect_err("second user should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
