//! Mistral AI chat-completions client.

use std::time::Duration;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

use crate::llm::client::BaseLLMClient;
use crate::llm::error::{Error, Result};
use crate::llm::types::{Message, Request, Response, ResponseStream, StreamChunk, Usage};
use crate::llm::utils::parse_finish_reason;

/// Base URL of the chat-completions endpoint.
const BASE_URL: &str = "https://api.mistral.ai/v1";

/// Name of the environment variable holding the API key.
const API_KEY_ENV: &str = "MISTRAL_API_KEY";

/// SSE terminator sent by the API to signal end-of-stream.
const DONE_MARKER: &str = "[DONE]";

/// Mistral AI chat-completions client.
pub struct Mistral {
    http: HttpClient,
}

impl Mistral {
    /// Create a new client with a fresh HTTP connection pool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: HttpClient::new(),
        }
    }
}

impl Default for Mistral {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseLLMClient for Mistral {
    fn name(&self) -> &'static str {
        "mistral"
    }

    fn api_key_env(&self) -> &'static str {
        API_KEY_ENV
    }

    async fn complete(&self, req: Request) -> Result<Response> {
        let api_key = std::env::var(API_KEY_ENV)
            .map_err(|_| Error::MissingApiKey(API_KEY_ENV.to_string()))?;

        let body = WireRequest {
            model: &req.model_name,
            messages: &req.messages,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
        };

        let resp = self
            .http
            .post(format!("{BASE_URL}/chat/completions"))
            .bearer_auth(&api_key)
            .timeout(Duration::from_secs(req.timeout_secs))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let wire: WireResponse = resp.json().await?;

        let first = wire.choices.into_iter().next().ok_or_else(|| Error::Api {
            status: status.as_u16(),
            body: "response contained no choices".into(),
        })?;

        Ok(Response {
            content: first.message.content,
            tool_calls: None,
            usage: Usage {
                prompt_tokens: wire.usage.prompt,
                completion_tokens: wire.usage.completion,
                total_tokens: wire.usage.total,
            },
            finish_reason: parse_finish_reason(&first.finish_reason),
        })
    }

    async fn stream(&self, req: Request) -> Result<ResponseStream> {
        let api_key = std::env::var(API_KEY_ENV)
            .map_err(|_| Error::MissingApiKey(API_KEY_ENV.to_string()))?;

        let body = WireStreamRequest {
            model: &req.model_name,
            messages: &req.messages,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let resp = self
            .http
            .post(format!("{BASE_URL}/chat/completions"))
            .bearer_auth(&api_key)
            .timeout(Duration::from_secs(req.timeout_secs))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                body: body_text,
            });
        }

        Ok(chunks(resp))
    }
}

/// Convert an SSE byte stream into a typed stream of [`StreamChunk`].
///
/// Reads `data: ...` lines and ignores everything else (blank separator
/// lines, SSE comment lines, unknown fields). Stops on the `[DONE]`
/// sentinel. Transport and JSON errors are propagated as the stream's
/// final item via `?`.
fn chunks(resp: reqwest::Response) -> ResponseStream {
    try_stream! {
        let mut bytes = resp.bytes_stream();
        let mut buf = Vec::<u8>::new();

        while let Some(chunk) = bytes.next().await {
            buf.extend_from_slice(&chunk?);

            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                let line = std::str::from_utf8(&line_bytes).unwrap_or("").trim_end();
                if line.is_empty() {
                    continue;
                }
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data == DONE_MARKER {
                    return;
                }
                let wire: WireStreamChunk = serde_json::from_str(data)?;
                yield convert_stream_chunk(wire);
            }
        }
    }
    .boxed()
}

/// Map a wire chunk into the public [`StreamChunk`] shape.
fn convert_stream_chunk(wire: WireStreamChunk) -> StreamChunk {
    let choice = wire.choices.into_iter().next();
    let (content_delta, finish_reason) = match choice {
        Some(c) => (
            c.delta.content,
            c.finish_reason.as_deref().map(parse_finish_reason),
        ),
        None => (None, None),
    };
    let usage = wire.usage.map(|u| Usage {
        prompt_tokens: u.prompt,
        completion_tokens: u.completion,
        total_tokens: u.total,
    });
    StreamChunk {
        content_delta,
        tool_call_deltas: None,
        finish_reason,
        usage,
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    max_tokens: u32,
    temperature: f32,
}

#[derive(Serialize)]
struct WireStreamRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    max_tokens: u32,
    temperature: f32,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireAssistantMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct WireAssistantMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct WireStreamChunk {
    choices: Vec<WireStreamChoice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireStreamChoice {
    delta: WireDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireDelta {
    content: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(rename = "prompt_tokens")]
    prompt: u32,
    #[serde(rename = "completion_tokens")]
    completion: u32,
    #[serde(rename = "total_tokens")]
    total: u32,
}
