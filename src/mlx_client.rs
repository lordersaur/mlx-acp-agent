use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::config::AppConfig;

// ---------------------------------------------------------------------------
// Wire-format message types
// ---------------------------------------------------------------------------

/// A single message in a chat completion request or response.
/// Mirrors the OpenAI chat message schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    /// `None` for assistant messages that only contain tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Present on assistant messages that contain tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    /// Present on `tool` role messages to correlate with the assistant call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_with_tool_calls(tool_calls: Vec<ChatToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// A tool call emitted by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatToolCallFunction {
    pub name: String,
    /// Arguments encoded as a JSON string (OpenAI wire format).
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// Structured result returned from the model
// ---------------------------------------------------------------------------

/// Parsed result from a single completion call.
#[derive(Debug, Clone)]
pub struct CompletionResult {
    /// The assistant's text response when no tool calls were emitted.
    pub content: Option<String>,
    /// Structured tool calls when the model chose to call tools.
    pub tool_calls: Vec<ApiToolCall>,
}

/// A single structured tool call ready for execution.
#[derive(Debug, Clone)]
pub struct ApiToolCall {
    /// Identifier used to correlate the result back to this call.
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
}

impl ApiToolCall {
    fn from_chat(c: &ChatToolCall) -> Option<Self> {
        let arguments: Value = serde_json::from_str(&c.function.arguments).ok()?;
        let arguments = match arguments {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        Some(Self {
            id: c.id.clone(),
            name: c.function.name.clone(),
            arguments,
        })
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MlxClient {
    client: Client,
    endpoint: String,
    model: String,
}

impl MlxClient {
    pub fn from_config(config: &AppConfig) -> Result<Self> {
        let client = Client::builder()
            .use_rustls_tls()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("failed to build reqwest client")?;

        Ok(Self {
            client,
            endpoint: config.mlx_url.clone(),
            model: config.mlx_model.clone(),
        })
    }

    pub async fn complete_raw(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        max_tokens: u32,
        temperature: f32,
    ) -> Result<CompletionResult> {
        let tools_field = if tools.is_empty() {
            None
        } else {
            Some(tools.to_vec())
        };

        let payload = CompletionRequest {
            model: self.model.clone(),
            stream: false,
            messages: messages.to_vec(),
            max_tokens,
            temperature,
            tools: tools_field,
        };

        let response = self
            .client
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("failed to call MLX endpoint {}", self.endpoint))?
            .error_for_status()
            .with_context(|| format!("MLX endpoint returned error for {}", self.endpoint))?;

        let body: CompletionResponse = response
            .json()
            .await
            .context("failed to decode MLX response JSON")?;

        let choice = body
            .choices
            .into_iter()
            .next()
            .context("MLX response had no choices")?;

        let msg = choice.message;

        // Prefer structured tool_calls if present.
        if let Some(chat_calls) = msg.tool_calls {
            let api_calls = chat_calls
                .iter()
                .filter_map(ApiToolCall::from_chat)
                .collect();
            let text = normalize_content(msg.content).trim().to_owned();
            return Ok(CompletionResult {
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls: api_calls,
            });
        }

        let text = normalize_content(msg.content).trim().to_owned();
        Ok(CompletionResult {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls: Vec::new(),
        })
    }

    /// Streaming variant: yields `<think>` tokens to `think_tx` as they arrive,
    /// then returns the full CompletionResult (with think tags stripped from content).
    pub async fn complete_streaming(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        max_tokens: u32,
        temperature: f32,
        think_tx: mpsc::UnboundedSender<String>,
    ) -> Result<CompletionResult> {
        let tools_field = if tools.is_empty() {
            None
        } else {
            Some(tools.to_vec())
        };

        let payload = CompletionRequest {
            model: self.model.clone(),
            stream: true,
            messages: messages.to_vec(),
            max_tokens,
            temperature,
            tools: tools_field,
        };

        let response = self
            .client
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("failed to call MLX endpoint {}", self.endpoint))?
            .error_for_status()
            .with_context(|| format!("MLX endpoint returned error for {}", self.endpoint))?;

        let mut byte_stream = response.bytes_stream();
        let mut full_text = String::new();
        let mut tool_calls_final: Vec<ChatToolCall> = Vec::new();
        let mut think_state = ThinkState::Before;
        let mut tag_buf = String::new();

        while let Some(item) = byte_stream.next().await {
            let bytes = item.context("error reading SSE stream")?;
            let text = String::from_utf8_lossy(&bytes);

            for line in text.lines() {
                let line = line.trim();
                if !line.starts_with("data: ") {
                    continue;
                }
                let payload = &line["data: ".len()..];
                if payload == "[DONE]" {
                    continue;
                }
                let data: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let choices = match data["choices"].as_array() {
                    Some(c) => c,
                    None => continue,
                };
                for choice in choices {
                    let delta = &choice["delta"];

                    // Streaming content delta — feed into think state machine.
                    if let Some(content) = delta["content"].as_str() {
                        if !content.is_empty() {
                            full_text.push_str(content);
                            stream_think_chunk(
                                &mut think_state,
                                &mut tag_buf,
                                content,
                                &think_tx,
                            );
                        }
                    }

                    // Tool calls arrive in the final delta chunk from main.py.
                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        for tc in tcs {
                            if let Ok(call) =
                                serde_json::from_value::<ChatToolCall>(tc.clone())
                            {
                                tool_calls_final.push(call);
                            }
                        }
                    }
                }
            }
        }

        // Strip think tags so agent_loop doesn't double-emit them.
        let content = extract_post_think(&full_text);

        if !tool_calls_final.is_empty() {
            let api_calls = tool_calls_final
                .iter()
                .filter_map(ApiToolCall::from_chat)
                .collect();
            // Keep only pre-tool-call text; the tool XML itself is not user-facing.
            let text = match content.find("<tool_call>") {
                Some(idx) => content[..idx].trim().to_owned(),
                None => content,
            };
            return Ok(CompletionResult {
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls: api_calls,
            });
        }

        Ok(CompletionResult {
            content: if content.is_empty() { None } else { Some(content) },
            tool_calls: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Think streaming helpers
// ---------------------------------------------------------------------------

enum ThinkState {
    Before,
    Inside,
    After,
}

/// Feed a delta token into the think state machine.
/// Sends content that falls inside `<think>…</think>` to `tx`.
///
/// Handles two patterns:
/// - Explicit: `<think>…content…</think>` (model emits the open tag)
/// - Pre-filled: `…content…</think>` (Qwen3.5 — open tag is in the prompt, not the output)
fn stream_think_chunk(
    state: &mut ThinkState,
    buf: &mut String,
    content: &str,
    tx: &mpsc::UnboundedSender<String>,
) {
    buf.push_str(content);
    loop {
        match state {
            ThinkState::Before => {
                let close_pos = buf.find("</think>");
                let open_pos = buf.find("<think>");
                let pre_filled = close_pos.map_or(false, |ci| open_pos.map_or(true, |oi| ci <= oi));
                if pre_filled {
                    // </think> comes before any <think> — pre-filled thinking (Qwen3.5).
                    let ci = close_pos.unwrap();
                    let thinking = buf[..ci].trim().to_owned();
                    if !thinking.is_empty() {
                        tx.send(thinking).ok();
                    }
                    *buf = buf[ci + "</think>".len()..].to_owned();
                    *state = ThinkState::After;
                    break;
                } else if let Some(oi) = open_pos {
                    // Explicit <think> tag — standard mode.
                    *buf = buf[oi + "<think>".len()..].to_owned();
                    *state = ThinkState::Inside;
                    // loop: check if </think> is already in buf
                } else {
                    // Neither tag seen yet — buffer and wait.
                    // If buffer grows too large the model isn't doing thinking (Fast mode).
                    if buf.len() > 4096 {
                        *state = ThinkState::After;
                        buf.clear();
                    }
                    break;
                }
            }
            ThinkState::Inside => {
                if let Some(idx) = buf.find("</think>") {
                    let before_close = buf[..idx].to_owned();
                    if !before_close.is_empty() {
                        tx.send(before_close).ok();
                    }
                    *buf = buf[idx + "</think>".len()..].to_owned();
                    *state = ThinkState::After;
                    break;
                } else {
                    // Flush all but the last 8 bytes (guards against a split `</think>`).
                    let safe = buf.len().saturating_sub(8);
                    let safe = floor_char_boundary(buf, safe);
                    if safe > 0 {
                        tx.send(buf[..safe].to_owned()).ok();
                        buf.drain(..safe);
                    }
                    break;
                }
            }
            ThinkState::After => {
                buf.clear();
                break;
            }
        }
    }
}

/// Return the text that follows the `</think>` closing tag (the actual answer).
fn extract_post_think(text: &str) -> String {
    if let Some(idx) = text.find("</think>") {
        text[idx + "</think>".len()..].trim().to_owned()
    } else if let Some(idx) = text.find("</thinking>") {
        text[idx + "</thinking>".len()..].trim().to_owned()
    } else {
        text.trim().to_owned()
    }
}


fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

// ---------------------------------------------------------------------------
// Request / response serde types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CompletionRequest {
    model: String,
    stream: bool,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
struct CompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    content: Option<Value>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatToolCall>>,
}

fn normalize_content(content: Option<Value>) -> String {
    match content {
        None => String::new(),
        Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text,
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                Value::Object(map) => map
                    .get("text")
                    .map(value_to_string)
                    .unwrap_or_else(|| Value::Object(map).to_string()),
                other => value_to_string(&other),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => value_to_string(&other),
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}
