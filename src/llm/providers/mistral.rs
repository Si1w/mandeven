//! Mistral AI chat-completions client.

use std::time::Duration;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

use crate::llm::client::BaseLLMClient;
use crate::llm::error::{Error, Result};
use crate::llm::types::{
    Message, Request, Response, ResponseStream, StreamChunk, Tool, ToolCall, ToolCallDelta, Usage,
};
use crate::llm::utils::parse_finish_reason;

/// Base URL of the chat-completions endpoint.
const BASE_URL: &str = "https://api.mistral.ai/v1";

/// Name of the environment variable holding the API key.
const API_KEY_ENV: &str = "MISTRAL_API_KEY";

/// SSE terminator sent by the API to signal end-of-stream.
const DONE_MARKER: &str = "[DONE]";

/// Wire-level value of the only tool kind Mistral accepts.
const TOOL_KIND_FUNCTION: &str = "function";

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

        let messages = wire_messages(&req.messages);
        let tools = wire_tools(&req.tools);
        let body = WireRequest {
            model: &req.model_name,
            messages: &messages,
            tools: &tools,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
        };

        let mut builder = self
            .http
            .post(format!("{BASE_URL}/chat/completions"))
            .bearer_auth(&api_key)
            .json(&body);
        if let Some(secs) = req.timeout_secs {
            builder = builder.timeout(Duration::from_secs(secs));
        }
        let resp = builder.send().await?;

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
            tool_calls: first
                .message
                .tool_calls
                .map(|calls| calls.into_iter().map(ToolCall::from).collect()),
            // Mistral has no thinking-mode capability; reasoning is
            // always absent in their wire response.
            thinking: None,
            usage: Usage {
                prompt_tokens: wire.usage.prompt,
                completion_tokens: wire.usage.completion,
                total_tokens: wire.usage.total,
                // Mistral does not surface prefix-cache accounting.
                cache_hit_tokens: None,
                cache_miss_tokens: None,
            },
            finish_reason: parse_finish_reason(&first.finish_reason),
        })
    }

    async fn stream(&self, req: Request) -> Result<ResponseStream> {
        let api_key = std::env::var(API_KEY_ENV)
            .map_err(|_| Error::MissingApiKey(API_KEY_ENV.to_string()))?;

        let messages = wire_messages(&req.messages);
        let tools = wire_tools(&req.tools);
        let body = WireStreamRequest {
            model: &req.model_name,
            messages: &messages,
            tools: &tools,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let mut builder = self
            .http
            .post(format!("{BASE_URL}/chat/completions"))
            .bearer_auth(&api_key)
            .json(&body);
        if let Some(secs) = req.timeout_secs {
            builder = builder.timeout(Duration::from_secs(secs));
        }
        let resp = builder.send().await?;

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
    let (content_delta, tool_call_deltas, finish_reason) = match wire.choices.into_iter().next() {
        Some(c) => {
            let finish = c.finish_reason.as_deref().map(parse_finish_reason);
            let tool_deltas = c
                .delta
                .tool_calls
                .map(|deltas| deltas.into_iter().map(ToolCallDelta::from).collect());
            (c.delta.content, tool_deltas, finish)
        }
        None => (None, None, None),
    };
    let usage = wire.usage.map(|u| Usage {
        prompt_tokens: u.prompt,
        completion_tokens: u.completion,
        total_tokens: u.total,
        // Mistral does not surface prefix-cache accounting.
        cache_hit_tokens: None,
        cache_miss_tokens: None,
    });
    StreamChunk {
        content_delta,
        // Mistral has no thinking-mode capability; thinking_delta is
        // always None on this wire path.
        thinking_delta: None,
        tool_call_deltas,
        finish_reason,
        usage,
    }
}

/// Project the public [`Message`] slice into the wire-shape the Mistral
/// API expects (OpenAI-compatible, with the `{type:"function",
/// function:{...}}` wrapper around tool calls).
fn wire_messages(messages: &[Message]) -> Vec<WireReqMessage<'_>> {
    messages.iter().map(WireReqMessage::from).collect()
}

/// Project the public [`Tool`] slice into the wire-shape Mistral
/// expects. Empty input yields an empty `Vec`, which the serializer
/// then omits from the request body.
fn wire_tools(tools: &[Tool]) -> Vec<WireReqTool<'_>> {
    tools.iter().map(WireReqTool::from).collect()
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [WireReqMessage<'a>],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [WireReqTool<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct WireStreamRequest<'a> {
    model: &'a str,
    messages: &'a [WireReqMessage<'a>],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [WireReqTool<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Wire-shape of one request-side message. Mirrors the public
/// [`Message`] enum but wraps tool calls in the OpenAI-style
/// `{type:"function", function:{...}}` envelope.
#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireReqMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: &'a str,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<WireReqToolCall<'a>>>,
    },
    Tool {
        content: &'a str,
        tool_call_id: &'a str,
    },
}

impl<'a> From<&'a Message> for WireReqMessage<'a> {
    fn from(m: &'a Message) -> Self {
        match m {
            Message::System { content } => WireReqMessage::System { content },
            Message::User { content } => WireReqMessage::User { content },
            // Mistral wire format has no slot for `reasoning`; the
            // field is dropped on serialize.
            Message::Assistant {
                content,
                tool_calls,
                reasoning: _,
            } => WireReqMessage::Assistant {
                content: content.as_deref(),
                tool_calls: tool_calls
                    .as_ref()
                    .map(|calls| calls.iter().map(WireReqToolCall::from).collect()),
            },
            Message::Tool {
                content,
                tool_call_id,
            } => WireReqMessage::Tool {
                content,
                tool_call_id,
            },
            // Compact boundary degrades to a system message — the
            // wire has no summary role.
            Message::Compact(boundary) => WireReqMessage::System {
                content: &boundary.summary,
            },
        }
    }
}

#[derive(Serialize)]
struct WireReqToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireReqFunctionCall<'a>,
}

impl<'a> From<&'a ToolCall> for WireReqToolCall<'a> {
    fn from(c: &'a ToolCall) -> Self {
        Self {
            id: &c.id,
            kind: TOOL_KIND_FUNCTION,
            function: WireReqFunctionCall {
                name: &c.name,
                arguments: &c.arguments,
            },
        }
    }
}

#[derive(Serialize)]
struct WireReqFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireReqTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireReqFunctionDef<'a>,
}

impl<'a> From<&'a Tool> for WireReqTool<'a> {
    fn from(t: &'a Tool) -> Self {
        Self {
            kind: TOOL_KIND_FUNCTION,
            function: WireReqFunctionDef {
                name: &t.name,
                description: &t.description,
                parameters: &t.parameters,
            },
        }
    }
}

#[derive(Serialize)]
struct WireReqFunctionDef<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
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
    #[serde(default)]
    tool_calls: Option<Vec<WireRespToolCall>>,
}

#[derive(Deserialize)]
struct WireRespToolCall {
    id: String,
    function: WireRespFunctionCall,
}

#[derive(Deserialize)]
struct WireRespFunctionCall {
    name: String,
    arguments: String,
}

impl From<WireRespToolCall> for ToolCall {
    fn from(w: WireRespToolCall) -> Self {
        Self {
            id: w.id,
            name: w.function.name,
            arguments: w.function.arguments,
        }
    }
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
    #[serde(default)]
    tool_calls: Option<Vec<WireDeltaToolCall>>,
}

#[derive(Deserialize)]
struct WireDeltaToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireDeltaFunctionCall>,
}

#[derive(Deserialize)]
struct WireDeltaFunctionCall {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl From<WireDeltaToolCall> for ToolCallDelta {
    fn from(w: WireDeltaToolCall) -> Self {
        let (name, arguments) = match w.function {
            Some(f) => (f.name, f.arguments),
            None => (None, None),
        };
        Self {
            index: w.index,
            id: w.id,
            name,
            arguments,
        }
    }
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
