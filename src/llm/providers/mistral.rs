//! Mistral AI chat-completions client.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

use crate::llm::client::BaseLLMClient;
use crate::llm::error::{Error, Result};
use crate::llm::types::{Message, Request, Response, ResponseStream, Usage};
use crate::llm::utils::parse_finish_reason;

/// Base URL of the chat-completions endpoint.
const BASE_URL: &str = "https://api.mistral.ai/v1";

/// Name of the environment variable holding the API key.
const API_KEY_ENV: &str = "MISTRAL_API_KEY";

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

    async fn stream(&self, _req: Request) -> Result<ResponseStream> {
        todo!("streaming deferred to the bus module design")
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    max_tokens: u32,
    temperature: f32,
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
struct WireUsage {
    #[serde(rename = "prompt_tokens")]
    prompt: u32,
    #[serde(rename = "completion_tokens")]
    completion: u32,
    #[serde(rename = "total_tokens")]
    total: u32,
}
