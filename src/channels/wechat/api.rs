//! Thin Tencent iLink Bot API client for the personal WeChat channel.

use std::io;
use std::time::Duration;

use qrcode::QrCode;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::sleep;
use uuid::Uuid;

/// Default iLink Bot API origin.
pub const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

const ILINK_APP_ID: &str = "bot";
const CHANNEL_VERSION: &str = "2.2.0";
const ILINK_APP_CLIENT_VERSION: u32 = (2 << 16) | (2 << 8);

const EP_GET_UPDATES: &str = "ilink/bot/getupdates";
const EP_SEND_MESSAGE: &str = "ilink/bot/sendmessage";
const EP_GET_BOT_QR: &str = "ilink/bot/get_bot_qrcode";
const EP_GET_QR_STATUS: &str = "ilink/bot/get_qrcode_status";

const QR_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// iLink credentials obtained from QR login.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WechatCredentials {
    /// iLink bot/account id.
    pub account_id: String,
    /// Bearer token for iLink Bot API calls.
    pub token: String,
    /// API base URL returned by iLink. Usually [`ILINK_BASE_URL`].
    pub base_url: String,
    /// WeChat user id reported by QR login, when available.
    #[serde(default)]
    pub user_id: String,
}

impl WechatCredentials {
    /// Return true when the minimum send/poll credentials are present.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.account_id.trim().is_empty() && !self.token.trim().is_empty()
    }
}

/// QR login session after the initial QR request.
#[derive(Debug, Clone)]
pub struct QrLoginStart {
    /// Short QR token used when polling status.
    pub qrcode: String,
    /// Full scannable URL when iLink returns one; otherwise `qrcode`.
    pub scan_data: String,
    /// API base URL to use for status polling.
    pub base_url: String,
}

/// One QR status-poll result.
#[derive(Debug, Clone)]
pub enum QrPoll {
    /// No user action observed yet.
    Waiting,
    /// The QR was scanned and the user still needs to confirm on the phone.
    Scanned,
    /// Polling should continue against a redirected host.
    Redirect { base_url: String },
    /// The QR expired.
    Expired,
    /// Login completed and yielded credentials.
    Confirmed(WechatCredentials),
}

/// Fetch a scannable iLink QR login code.
///
/// # Errors
///
/// Returns an I/O-shaped error for HTTP, JSON, or malformed-response
/// failures so callers can reuse channel error plumbing.
pub async fn request_qr(client: &Client, base_url: &str) -> io::Result<QrLoginStart> {
    let raw = api_get(
        client,
        base_url,
        &format!("{EP_GET_BOT_QR}?bot_type=3"),
        Duration::from_secs(35),
    )
    .await?;
    let qrcode = raw
        .get("qrcode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if qrcode.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "iLink QR response missing qrcode",
        ));
    }
    let scan_data = raw
        .get("qrcode_img_content")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&qrcode)
        .trim()
        .to_string();
    Ok(QrLoginStart {
        qrcode,
        scan_data,
        base_url: base_url.trim_end_matches('/').to_string(),
    })
}

/// Poll one QR login status step.
pub async fn poll_qr_once(client: &Client, base_url: &str, qrcode: &str) -> io::Result<QrPoll> {
    let raw = api_get(
        client,
        base_url,
        &format!("{EP_GET_QR_STATUS}?qrcode={qrcode}"),
        Duration::from_secs(35),
    )
    .await?;
    let status = raw
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("wait")
        .trim();
    match status {
        "wait" => Ok(QrPoll::Waiting),
        "scaned" => Ok(QrPoll::Scanned),
        "scaned_but_redirect" => {
            let host = raw
                .get("redirect_host")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            if host.is_empty() {
                Ok(QrPoll::Waiting)
            } else {
                Ok(QrPoll::Redirect {
                    base_url: format!("https://{host}"),
                })
            }
        }
        "expired" => Ok(QrPoll::Expired),
        "confirmed" => {
            let account_id = raw
                .get("ilink_bot_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let token = raw
                .get("bot_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if account_id.is_empty() || token.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "iLink QR confirmed but credential payload was incomplete",
                ));
            }
            let base_url = raw
                .get("baseurl")
                .and_then(Value::as_str)
                .unwrap_or(ILINK_BASE_URL)
                .trim()
                .trim_end_matches('/')
                .to_string();
            let user_id = raw
                .get("ilink_user_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            Ok(QrPoll::Confirmed(WechatCredentials {
                account_id,
                token,
                base_url,
                user_id,
            }))
        }
        _ => Ok(QrPoll::Waiting),
    }
}

/// Poll until QR login finishes or times out.
pub async fn wait_for_qr_confirmation(
    client: &Client,
    start: &QrLoginStart,
    timeout: Duration,
) -> io::Result<WechatCredentials> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut base_url = start.base_url.clone();
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "WeChat QR login timed out",
            ));
        }
        match poll_qr_once(client, &base_url, &start.qrcode).await? {
            QrPoll::Waiting | QrPoll::Scanned => {}
            QrPoll::Redirect {
                base_url: redirected,
            } => base_url = redirected,
            QrPoll::Expired => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "WeChat QR code expired; run /wechat login again",
                ));
            }
            QrPoll::Confirmed(creds) => return Ok(creds),
        }
        sleep(QR_POLL_INTERVAL).await;
    }
}

/// Render a QR code as terminal-friendly block text.
#[must_use]
pub fn render_qr_ascii(data: &str) -> String {
    match QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<char>()
            .quiet_zone(true)
            .module_dimensions(2, 1)
            .dark_color('█')
            .light_color(' ')
            .build(),
        Err(_) => data.to_string(),
    }
}

/// Long-poll iLink for inbound messages.
pub async fn get_updates(
    client: &Client,
    credentials: &WechatCredentials,
    sync_buf: &str,
    timeout: Duration,
) -> io::Result<Value> {
    api_post(
        client,
        &credentials.base_url,
        EP_GET_UPDATES,
        json!({ "get_updates_buf": sync_buf }),
        Some(&credentials.token),
        timeout,
    )
    .await
}

/// Send one text message through iLink.
pub async fn send_text(
    client: &Client,
    credentials: &WechatCredentials,
    to: &str,
    text: &str,
    context_token: Option<&str>,
    client_id: &str,
) -> io::Result<Value> {
    if text.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WeChat text message must not be empty",
        ));
    }
    let mut msg = json!({
        "from_user_id": "",
        "to_user_id": to,
        "client_id": client_id,
        "message_type": 2,
        "message_state": 2,
        "item_list": [{
            "type": 1,
            "text_item": { "text": text },
        }],
    });
    if let Some(token) = context_token.filter(|s| !s.trim().is_empty())
        && let Some(obj) = msg.as_object_mut()
    {
        obj.insert(
            "context_token".to_string(),
            Value::String(token.to_string()),
        );
    }
    api_post(
        client,
        &credentials.base_url,
        EP_SEND_MESSAGE,
        json!({ "msg": msg }),
        Some(&credentials.token),
        Duration::from_secs(15),
    )
    .await
}

async fn api_get(
    client: &Client,
    base_url: &str,
    endpoint: &str,
    timeout: Duration,
) -> io::Result<Value> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
    let response = client
        .get(url)
        .header("iLink-App-Id", ILINK_APP_ID)
        .header(
            "iLink-App-ClientVersion",
            ILINK_APP_CLIENT_VERSION.to_string(),
        )
        .timeout(timeout)
        .send()
        .await
        .map_err(io_other)?;
    let status = response.status();
    let raw = response.text().await.map_err(io_other)?;
    if !status.is_success() {
        return Err(io::Error::other(format!(
            "iLink GET {endpoint} HTTP {status}: {}",
            truncate_for_error(&raw)
        )));
    }
    serde_json::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

async fn api_post(
    client: &Client,
    base_url: &str,
    endpoint: &str,
    payload: Value,
    token: Option<&str>,
    timeout: Duration,
) -> io::Result<Value> {
    let mut body = payload;
    let obj = body.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "iLink POST body must be an object",
        )
    })?;
    obj.insert(
        "base_info".to_string(),
        json!({ "channel_version": CHANNEL_VERSION }),
    );
    let body_text = serde_json::to_string(&body)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("Content-Length", body_text.len().to_string())
        .header("X-WECHAT-UIN", random_wechat_uin())
        .header("iLink-App-Id", ILINK_APP_ID)
        .header(
            "iLink-App-ClientVersion",
            ILINK_APP_CLIENT_VERSION.to_string(),
        )
        .timeout(timeout)
        .body(body_text);
    if let Some(token) = token.filter(|s| !s.trim().is_empty()) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.map_err(io_other)?;
    let status = response.status();
    let raw = response.text().await.map_err(io_other)?;
    if !status.is_success() {
        return Err(io::Error::other(format!(
            "iLink POST {endpoint} HTTP {status}: {}",
            truncate_for_error(&raw)
        )));
    }
    serde_json::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn random_wechat_uin() -> String {
    let value = (Uuid::now_v7().as_u128() & u128::from(u32::MAX)) as u32;
    base64_encode(value.to_string().as_bytes())
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn io_other(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

fn truncate_for_error(raw: &str) -> String {
    raw.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_helper_matches_known_value() {
        assert_eq!(base64_encode(b"1234"), "MTIzNA==");
    }
}
