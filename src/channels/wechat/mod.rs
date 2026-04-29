//! WeChat channel — text-only personal WeChat adapter via iLink.
//!
//! Scope (MS0):
//!
//! - Personal WeChat/iLink route, not WeCom.
//! - Direct messages only. Group chats are dropped because the current
//!   gateway binds one active session per channel.
//! - Runtime-mutable single-user allowlist, persisted under
//!   `<data_dir>/channels/wechat/allowlist.json`.
//! - QR login persists account credentials under
//!   `<data_dir>/channels/wechat/accounts/<account_id>.json`.
//! - Text-only inbound/outbound. Media support in Hermes requires an
//!   encrypted CDN flow and a richer bus payload, so it is intentionally
//!   out of scope for this first channel.
//! - WeChat cannot edit sent messages; `ReplyDelta` fragments are
//!   buffered and sent once on `ReplyEnd`.

pub mod api;
mod control;
mod state;
pub mod store;

pub use control::{WechatControl, WechatLogin, WechatStatus};
pub use store::{ALLOWLIST_FILENAME, CHANNELS_SUBDIR, WECHAT_SUBDIR, allowlist_path};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use crate::bus::{ChannelID, InboundMessage, InboundPayload, InboundSender};
use crate::bus::{OutboundMessage, OutboundPayload};
use crate::channels::Channel;
use crate::channels::common::{AllowList, split_message};
use crate::channels::error::{Error, Result};
use crate::config::WechatConfig;

use self::api::WechatCredentials;
use self::state::WechatState;

const WECHAT_MAX_MESSAGE_LEN: usize = 4000;
const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(35);
const RETRY_DELAY: Duration = Duration::from_secs(2);
const BACKOFF_DELAY: Duration = Duration::from_secs(30);
const SEND_CHUNK_DELAY: Duration = Duration::from_millis(350);
const SESSION_EXPIRED_ERRCODE: i64 = -14;

/// WeChat channel implementing [`Channel`].
pub struct WechatChannel {
    id: ChannelID,
    data_dir: PathBuf,
    state: Arc<WechatState>,
}

impl WechatChannel {
    /// Construct a channel and paired runtime control handle.
    #[must_use]
    pub fn build(
        id: ChannelID,
        cfg: &WechatConfig,
        initial_allowed: impl IntoIterator<Item = String>,
        allowlist_path: PathBuf,
        data_dir: PathBuf,
    ) -> (Self, WechatControl) {
        let allowed = AllowList::with_initial(initial_allowed);
        let (active_tx, _rx) = watch::channel(false);
        let active = Arc::new(active_tx);
        let credentials: Arc<Mutex<Option<WechatCredentials>>> = Arc::new(Mutex::new(None));
        let state = Arc::new(WechatState::new(
            allowed.clone(),
            active.clone(),
            credentials.clone(),
        ));
        let channel = Self {
            id,
            data_dir: data_dir.clone(),
            state,
        };
        let control = WechatControl::new(
            allowed,
            allowlist_path,
            data_dir,
            active,
            credentials,
            cfg.clone(),
        );
        (channel, control)
    }
}

#[async_trait]
impl Channel for WechatChannel {
    fn id(&self) -> &ChannelID {
        &self.id
    }

    async fn start(&self, inbound: InboundSender) -> Result<()> {
        let mut active_rx = self.state.active.subscribe();
        loop {
            while !*active_rx.borrow_and_update() {
                if active_rx.changed().await.is_err() {
                    return Ok(());
                }
            }

            let Some(credentials) = self.state.credentials.lock().await.clone() else {
                eprintln!("[wechat] enable signaled without credentials; resetting to disabled");
                let _ = self.state.active.send(false);
                continue;
            };

            if let Err(err) = self
                .run_session(credentials, &inbound, &mut active_rx)
                .await
            {
                eprintln!("[wechat] session ended: {err}");
            }
            self.clear_session_state().await;
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let Some(client) = self.state.client.lock().await.clone() else {
            return Ok(());
        };
        let Some(credentials) = self.state.credentials.lock().await.clone() else {
            return Ok(());
        };

        match msg.payload {
            OutboundPayload::TurnEnd | OutboundPayload::ThinkingDelta { .. } => Ok(()),
            OutboundPayload::Reply(text) => {
                self.send_to_current_peer(&client, &credentials, &text)
                    .await
            }
            OutboundPayload::ReplyDelta { stream_id, delta } => {
                self.state
                    .streams
                    .lock()
                    .await
                    .entry(stream_id)
                    .or_default()
                    .push_str(&delta);
                Ok(())
            }
            OutboundPayload::ReplyEnd { stream_id } => {
                let text = self.state.streams.lock().await.remove(&stream_id);
                if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                    self.send_to_current_peer(&client, &credentials, &text)
                        .await?;
                }
                Ok(())
            }
            OutboundPayload::Notice(text) => {
                self.send_to_current_peer(&client, &credentials, &format!("[notice] {text}"))
                    .await
            }
            OutboundPayload::Error(text) => {
                self.send_to_current_peer(&client, &credentials, &format!("[error]\n{text}"))
                    .await
            }
            OutboundPayload::SessionSwitched(_) => {
                self.send_to_current_peer(&client, &credentials, "[session switched]")
                    .await
            }
        }
    }
}

impl WechatChannel {
    async fn run_session(
        &self,
        credentials: WechatCredentials,
        inbound: &InboundSender,
        active_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let client = Client::new();
        *self.state.client.lock().await = Some(client.clone());
        *self.state.context_tokens.lock().await =
            store::load_context_tokens(&self.data_dir, &credentials.account_id)
                .await
                .map_err(Error::from)?;

        let mut sync_buf = store::load_sync_buf(&self.data_dir, &credentials.account_id)
            .await
            .map_err(Error::from)?;
        let mut consecutive_failures = 0usize;

        loop {
            let response = tokio::select! {
                () = wait_for_inactive(active_rx) => return Ok(()),
                response = api::get_updates(&client, &credentials, &sync_buf, LONG_POLL_TIMEOUT) => response,
            };

            match response {
                Ok(value) => {
                    if let Some((code, message)) = ilink_error(&value) {
                        if code == SESSION_EXPIRED_ERRCODE {
                            eprintln!(
                                "[wechat] session expired for account {}; run /wechat login again",
                                safe_id(&credentials.account_id)
                            );
                            let _ = self.state.active.send(false);
                            return Ok(());
                        }
                        consecutive_failures += 1;
                        eprintln!("[wechat] getupdates failed ({consecutive_failures}): {message}");
                        sleep_with_active(active_rx, retry_delay(consecutive_failures)).await;
                        continue;
                    }

                    consecutive_failures = 0;
                    if let Some(next) = value.get("get_updates_buf").and_then(Value::as_str)
                        && !next.is_empty()
                    {
                        sync_buf = next.to_string();
                        store::save_sync_buf(&self.data_dir, &credentials.account_id, &sync_buf)
                            .await
                            .map_err(Error::from)?;
                    }

                    if let Some(messages) = value.get("msgs").and_then(Value::as_array) {
                        for message in messages {
                            self.handle_inbound(&credentials, inbound, message).await?;
                        }
                    }
                }
                Err(err) => {
                    consecutive_failures += 1;
                    eprintln!("[wechat] poll error ({consecutive_failures}): {err}");
                    sleep_with_active(active_rx, retry_delay(consecutive_failures)).await;
                }
            }
        }
    }

    async fn handle_inbound(
        &self,
        credentials: &WechatCredentials,
        inbound: &InboundSender,
        message: &Value,
    ) -> Result<()> {
        let sender_id = string_field(message, "from_user_id");
        if sender_id.is_empty() || sender_id == credentials.account_id {
            return Ok(());
        }

        let message_id = string_field(message, "message_id");
        if !message_id.is_empty() {
            let mut seen = self.state.seen_messages.lock().await;
            if !seen.insert(message_id) {
                return Ok(());
            }
        }

        let (chat_type, effective_chat_id) = guess_chat(message, &credentials.account_id);
        if chat_type == "group" {
            return Ok(());
        }
        if !self.state.allowed.is_allowed(&sender_id) {
            eprintln!(
                "[wechat] dropped unauthorized DM from {}",
                safe_id(&sender_id)
            );
            return Ok(());
        }

        let context_token = string_field(message, "context_token");
        if !context_token.is_empty() {
            let snapshot = {
                let mut tokens = self.state.context_tokens.lock().await;
                tokens.insert(sender_id.clone(), context_token);
                tokens.clone()
            };
            store::save_context_tokens(&self.data_dir, &credentials.account_id, &snapshot)
                .await
                .map_err(Error::from)?;
        }

        let text = extract_text(message.get("item_list").and_then(Value::as_array));
        if text.trim().is_empty() {
            return Ok(());
        }

        *self.state.reply_peer.lock().await = Some(effective_chat_id);
        let payload = if let Some(command) = text.strip_prefix('/') {
            InboundPayload::Command(command.to_string())
        } else {
            InboundPayload::UserInput(text)
        };
        let mut msg = InboundMessage::with_peer(self.id.clone(), sender_id, payload);
        msg.account_id = Some(credentials.account_id.clone());
        inbound.send(msg).await.map_err(Error::from)
    }

    async fn send_to_current_peer(
        &self,
        client: &Client,
        credentials: &WechatCredentials,
        text: &str,
    ) -> Result<()> {
        let Some(peer) = self.state.reply_peer.lock().await.clone() else {
            return Ok(());
        };
        self.send_text(client, credentials, &peer, text).await
    }

    async fn send_text(
        &self,
        client: &Client,
        credentials: &WechatCredentials,
        peer: &str,
        text: &str,
    ) -> Result<()> {
        let chunks: Vec<String> = split_message(text, WECHAT_MAX_MESSAGE_LEN)
            .into_iter()
            .filter(|chunk| !chunk.trim().is_empty())
            .collect();
        let mut context_token = self.state.context_tokens.lock().await.get(peer).cloned();

        for (idx, chunk) in chunks.iter().enumerate() {
            let client_id = format!("mandeven-wechat-{}", Uuid::now_v7());
            let mut response = api::send_text(
                client,
                credentials,
                peer,
                chunk,
                context_token.as_deref(),
                &client_id,
            )
            .await
            .map_err(Error::from)?;

            if ilink_error_code(&response) == Some(SESSION_EXPIRED_ERRCODE)
                && context_token.is_some()
            {
                context_token = None;
                response = api::send_text(client, credentials, peer, chunk, None, &client_id)
                    .await
                    .map_err(Error::from)?;
            }
            if let Some((_code, message)) = ilink_error(&response) {
                return Err(Error::Io(std::io::Error::other(format!(
                    "WeChat send failed: {message}"
                ))));
            }
            if idx < chunks.len() - 1 {
                tokio::time::sleep(SEND_CHUNK_DELAY).await;
            }
        }
        Ok(())
    }

    async fn clear_session_state(&self) {
        *self.state.client.lock().await = None;
        *self.state.reply_peer.lock().await = None;
        self.state.streams.lock().await.clear();
    }
}

async fn wait_for_inactive(rx: &mut watch::Receiver<bool>) {
    loop {
        if !*rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn sleep_with_active(rx: &mut watch::Receiver<bool>, duration: Duration) {
    tokio::select! {
        () = tokio::time::sleep(duration) => {}
        () = wait_for_inactive(rx) => {}
    }
}

fn retry_delay(consecutive_failures: usize) -> Duration {
    if consecutive_failures >= 3 {
        BACKOFF_DELAY
    } else {
        RETRY_DELAY
    }
}

fn ilink_error_code(value: &Value) -> Option<i64> {
    ["ret", "errcode"].into_iter().find_map(|field| {
        let code = value.get(field).and_then(Value::as_i64)?;
        (code != 0).then_some(code)
    })
}

fn ilink_error(value: &Value) -> Option<(i64, String)> {
    let code = ilink_error_code(value)?;
    let msg = value
        .get("errmsg")
        .or_else(|| value.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("unknown iLink error");
    Some((code, format!("ret/errcode={code} errmsg={msg}")))
}

fn guess_chat(message: &Value, account_id: &str) -> (&'static str, String) {
    let room_id = string_field(message, "room_id");
    let chat_room_id = string_field(message, "chat_room_id");
    let to_user_id = string_field(message, "to_user_id");
    let msg_type = message.get("msg_type").and_then(Value::as_i64);
    let is_group = !room_id.is_empty()
        || !chat_room_id.is_empty()
        || (!to_user_id.is_empty() && to_user_id != account_id && msg_type == Some(1));
    if is_group {
        (
            "group",
            first_non_empty(&[
                room_id,
                chat_room_id,
                to_user_id,
                string_field(message, "from_user_id"),
            ]),
        )
    } else {
        ("dm", string_field(message, "from_user_id"))
    }
}

fn extract_text(items: Option<&Vec<Value>>) -> String {
    let Some(items) = items else {
        return String::new();
    };
    for item in items {
        if item.get("type").and_then(Value::as_i64) == Some(1) {
            return item
                .get("text_item")
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
    }
    for item in items {
        if item.get("type").and_then(Value::as_i64) == Some(4) {
            let text = item
                .get("voice_item")
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn first_non_empty(values: &[String]) -> String {
    values
        .iter()
        .find(|v| !v.trim().is_empty())
        .cloned()
        .unwrap_or_default()
}

fn safe_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= 8 {
        trimmed.to_string()
    } else {
        trimmed.chars().take(8).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_text, guess_chat};
    use serde_json::json;

    #[test]
    fn extracts_text_item() {
        let msg = json!({
            "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
        });
        assert_eq!(
            extract_text(msg.get("item_list").and_then(|v| v.as_array())),
            "hello"
        );
    }

    #[test]
    fn guesses_dm_chat() {
        let msg = json!({"from_user_id": "wxid_a", "to_user_id": "acct"});
        assert_eq!(guess_chat(&msg, "acct"), ("dm", "wxid_a".to_string()));
    }
}
