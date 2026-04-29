use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::model_parser::{clean_streaming_answer, clean_streaming_chunk, extract_thought_blocks};

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
            top_p: Some(0.95),
            tools: tools_field,
            extra_body: Some(gemma4_extra_body(messages)),
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
            let text = clean_model_text(&normalize_content(msg.content));
            return Ok(CompletionResult {
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls: api_calls,
            });
        }

        let text = clean_model_text(&normalize_content(msg.content));
        Ok(CompletionResult {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls: Vec::new(),
        })
    }

    /// Streaming variant: yields Gemma thinking tokens to `think_tx` as they arrive,
    /// then returns the full CompletionResult (with think tags stripped from content).
    pub async fn complete_streaming(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        max_tokens: u32,
        temperature: f32,
        think_tx: mpsc::UnboundedSender<String>,
        answer_tx: Option<mpsc::UnboundedSender<String>>,
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
            top_p: Some(0.95),
            tools: tools_field,
            extra_body: Some(gemma4_extra_body(messages)),
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
        let mut content_stream = ContentStreamState::default();
        let mut answer_stream = AnswerStreamState::default();
        let mut pending_sse_line = String::new();

        while let Some(item) = byte_stream.next().await {
            let bytes = item.context("error reading SSE stream")?;
            let text = String::from_utf8_lossy(&bytes);

            consume_sse_chunk(&mut pending_sse_line, &text, |line| {
                let line = line.trim();
                if !line.starts_with("data: ") {
                    return;
                }
                let payload = &line["data: ".len()..];
                if payload == "[DONE]" {
                    return;
                }
                let data: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let choices = match data["choices"].as_array() {
                    Some(c) => c,
                    None => return,
                };
                for choice in choices {
                    let delta = &choice["delta"];

                    // Streaming content delta — feed into think state machine.
                    if let Some(content) = delta["content"].as_str() {
                        if !content.is_empty() {
                            full_text.push_str(content);
                            for event in stream_content_chunk(&mut content_stream, content) {
                                match event {
                                    StreamEvent::Thought(chunk) => {
                                        think_tx.send(chunk).ok();
                                    }
                                    StreamEvent::Answer(chunk) => {
                                        if let Some(ref tx) = answer_tx {
                                            stream_answer_chunk(&mut answer_stream, &chunk, tx);
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Tool calls arrive in the final delta chunk from main.py.
                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        for tc in tcs {
                            if let Ok(call) = serde_json::from_value::<ChatToolCall>(tc.clone()) {
                                tool_calls_final.push(call);
                            }
                        }
                    }
                }
            });
        }

        if !pending_sse_line.trim().is_empty() {
            let line = pending_sse_line.trim();
            if line.starts_with("data: ") {
                let payload = &line["data: ".len()..];
                if payload != "[DONE]" {
                    let data: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => Value::Null,
                    };
                    if let Some(choices) = data["choices"].as_array() {
                        for choice in choices {
                            let delta = &choice["delta"];

                            if let Some(content) = delta["content"].as_str() {
                                if !content.is_empty() {
                                    full_text.push_str(content);
                                    for event in stream_content_chunk(&mut content_stream, content)
                                    {
                                        match event {
                                            StreamEvent::Thought(chunk) => {
                                                think_tx.send(chunk).ok();
                                            }
                                            StreamEvent::Answer(chunk) => {
                                                if let Some(ref tx) = answer_tx {
                                                    stream_answer_chunk(
                                                        &mut answer_stream,
                                                        &chunk,
                                                        tx,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }

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
            }
        }

        let content = clean_model_text(&full_text);

        if !tool_calls_final.is_empty() {
            for event in finish_content_stream(&mut content_stream) {
                if let StreamEvent::Thought(chunk) = event {
                    think_tx.send(chunk).ok();
                }
            }

            let api_calls = tool_calls_final
                .iter()
                .filter_map(ApiToolCall::from_chat)
                .collect();
            return Ok(CompletionResult {
                content: if content.is_empty() {
                    None
                } else {
                    Some(content)
                },
                tool_calls: api_calls,
            });
        }

        if let Some(ref tx) = answer_tx {
            for event in finish_content_stream(&mut content_stream) {
                if let StreamEvent::Answer(chunk) = event {
                    stream_answer_chunk(&mut answer_stream, &chunk, tx);
                }
            }
            flush_answer_stream(&mut answer_stream, tx);
        }

        Ok(CompletionResult {
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
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

impl Default for ThinkState {
    fn default() -> Self {
        Self::Before
    }
}

#[derive(Default)]
struct ContentStreamState {
    thought: ThinkState,
    buf: String,
    /// True while streaming a secondary thought block (re-opened after the
    /// first close tag). Content is discarded so it does not appear in Zed
    /// as a duplicate thought panel.
    secondary: bool,
}

#[derive(Default)]
struct AnswerStreamState {
    buf: String,
}

enum StreamEvent {
    Thought(String),
    Answer(String),
}

/// Feed a delta token into the think state machine.
/// Splits content into thinking chunks and visible-answer chunks.
///
/// Handles Gemma thinking patterns:
/// - Gemma explicit: `<|think|>…content…<|/think|>`
/// - Gemma explicit: `<|channel>thought\n…content…<channel|>`
/// - Gemma pre-filled: `…content…<channel|>`
fn stream_content_chunk(state: &mut ContentStreamState, content: &str) -> Vec<StreamEvent> {
    const OPEN_GEMMA_THINK: &str = "<|think|>";
    const CLOSE_GEMMA_THINK: &str = "<|/think|>";
    const OPEN_CHANNEL: &str = "<|channel>thought";
    const CLOSE_CHANNEL: &str = "<channel|>";
    const MAX_TAG_LEN: usize = OPEN_CHANNEL.len();

    let mut events = Vec::new();
    state.buf.push_str(content);
    loop {
        match state.thought {
            ThinkState::Before => {
                let earliest_close = earliest_tag(&state.buf, &[CLOSE_GEMMA_THINK, CLOSE_CHANNEL]);
                let earliest_open = earliest_tag(&state.buf, &[OPEN_GEMMA_THINK, OPEN_CHANNEL]);

                let pre_filled = earliest_close.map_or(false, |(ci, _)| {
                    earliest_open.map_or(true, |(oi, _)| ci <= oi)
                });

                if pre_filled {
                    let (ci, close_tag) = earliest_close.unwrap();
                    let thinking = state.buf[..ci].trim().to_owned();
                    if !thinking.is_empty() {
                        events.push(StreamEvent::Thought(thinking));
                    }
                    let answer = state.buf[ci + close_tag.len()..].to_owned();
                    state.buf.clear();
                    state.thought = ThinkState::After;
                    if !answer.is_empty() {
                        events.push(StreamEvent::Answer(answer));
                    }
                } else if let Some((oi, open_tag)) = earliest_open {
                    let prefix = state.buf[..oi].to_owned();
                    if !prefix.trim().is_empty() {
                        events.push(StreamEvent::Answer(prefix));
                    }
                    state.buf = state.buf[oi + open_tag.len()..].to_owned();
                    strip_leading_channel_separator(&mut state.buf);
                    state.thought = ThinkState::Inside;
                    // loop: check if close tag is already in buf
                } else {
                    // Buffer until we are reasonably sure it's not an initial tag.
                    // This avoids leaking the first few words of a thought that arrives
                    // before its tag (or a pre-filled thought).
                    let wait_limit = 100;
                    if state.buf.len() < wait_limit {
                        break;
                    }
                    let safe = state.buf.len().saturating_sub(MAX_TAG_LEN - 1);
                    let safe = floor_char_boundary(&state.buf, safe);
                    if safe > 0 {
                        events.push(StreamEvent::Answer(state.buf[..safe].to_owned()));
                        state.buf.drain(..safe);
                    }
                    break;
                }
            }
            ThinkState::Inside => {
                let close = earliest_tag(&state.buf, &[CLOSE_GEMMA_THINK, CLOSE_CHANNEL]);
                if let Some((idx, tag)) = close {
                    let before_close = state.buf[..idx].to_owned();
                    // Only emit thought content for the primary (first) block.
                    if !before_close.is_empty() && !state.secondary {
                        events.push(StreamEvent::Thought(before_close));
                    }
                    let answer = state.buf[idx + tag.len()..].to_owned();
                    state.buf.clear();
                    state.secondary = false;
                    state.thought = ThinkState::After;
                    if !answer.is_empty() {
                        events.push(StreamEvent::Answer(answer));
                    }
                } else {
                    // Flush all but the last 12 bytes (guards against a split close tag).
                    let safe = state.buf.len().saturating_sub(12);
                    let safe = floor_char_boundary(&state.buf, safe);
                    if safe > 0 {
                        // Only emit thought content for the primary (first) block.
                        if !state.secondary {
                            events.push(StreamEvent::Thought(state.buf[..safe].to_owned()));
                        }
                        state.buf.drain(..safe);
                    }
                    break;
                }
            }
            ThinkState::After => {
                // Handle stray thought blocks the model re-opens after the first close.
                let earliest_open = earliest_tag(&state.buf, &[OPEN_GEMMA_THINK, OPEN_CHANNEL]);
                if let Some((oi, open_tag)) = earliest_open {
                    let open_end = oi + open_tag.len();
                    let mut inner = state.buf[open_end..].to_owned();
                    strip_leading_channel_separator(&mut inner);
                    if let Some((ci, close_tag)) =
                        earliest_tag(&inner, &[CLOSE_GEMMA_THINK, CLOSE_CHANNEL])
                    {
                        // Balanced secondary block: emit prefix as answer, discard body
                        // (do NOT send to think_tx — prevents a duplicate thought panel in Zed).
                        if oi > 0 {
                            events.push(StreamEvent::Answer(state.buf[..oi].to_owned()));
                        }
                        state.buf = inner[ci + close_tag.len()..].to_owned();
                        // loop: remain in After, handle whatever follows
                    } else {
                        // Open tag but no close yet — emit prefix as answer, then
                        // transition to Inside marked as secondary so its content is discarded.
                        if oi > 0 {
                            events.push(StreamEvent::Answer(state.buf[..oi].to_owned()));
                        }
                        state.buf = state.buf[oi + open_tag.len()..].to_owned();
                        strip_leading_channel_separator(&mut state.buf);
                        state.secondary = true;
                        state.thought = ThinkState::Inside;
                        // loop: will handle Inside state in next iteration
                    }
                } else {
                    // No open tags — safe-emit the remainder as answer.
                    let safe = state.buf.len().saturating_sub(MAX_TAG_LEN - 1);
                    let safe = floor_char_boundary(&state.buf, safe);
                    if safe > 0 {
                        events.push(StreamEvent::Answer(state.buf[..safe].to_owned()));
                        state.buf.drain(..safe);
                    }
                    break;
                }
            }
        }
    }
    events
}

fn finish_content_stream(state: &mut ContentStreamState) -> Vec<StreamEvent> {
    match state.thought {
        ThinkState::Inside => {
            if state.buf.is_empty() {
                Vec::new()
            } else {
                vec![StreamEvent::Thought(std::mem::take(&mut state.buf))]
            }
        }
        ThinkState::Before | ThinkState::After => {
            if state.buf.is_empty() {
                Vec::new()
            } else {
                vec![StreamEvent::Answer(std::mem::take(&mut state.buf))]
            }
        }
    }
}

fn stream_answer_chunk(
    _state: &mut AnswerStreamState,
    content: &str,
    tx: &mpsc::UnboundedSender<String>,
) {
    if content.is_empty() {
        return;
    }
    // Strip any stray thought markers from the chunk without trimming so
    // whitespace tokens ("Hello ") are preserved across the stream.
    let text = clean_streaming_chunk(content);
    if !text.is_empty() {
        tx.send(text).ok();
    }
}

fn consume_sse_chunk<F>(pending: &mut String, chunk: &str, mut on_line: F)
where
    F: FnMut(&str),
{
    if !chunk.is_empty() {
        pending.push_str(chunk);
    }

    while let Some(newline) = pending.find('\n') {
        let line: String = pending.drain(..=newline).collect();
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        on_line(line);
    }
}

fn flush_answer_stream(state: &mut AnswerStreamState, tx: &mpsc::UnboundedSender<String>) {
    if !state.buf.is_empty() {
        let text = clean_streaming_answer(&std::mem::take(&mut state.buf));
        if !text.is_empty() {
            tx.send(text).ok();
        }
    }
}

/// Return the text that follows the thinking closing tag (the actual answer).
#[allow(dead_code)]
fn extract_post_think(text: &str) -> String {
    clean_model_text(text)
}

fn clean_model_text(text: &str) -> String {
    let (_, cleaned) = extract_thought_blocks(text);
    cleaned.trim().to_owned()
}

fn earliest_tag<'a>(text: &str, tags: &[&'a str]) -> Option<(usize, &'a str)> {
    tags.iter()
        .filter_map(|tag| text.find(tag).map(|idx| (idx, *tag)))
        .min_by_key(|(idx, _)| *idx)
}

fn strip_leading_channel_separator(buf: &mut String) {
    while let Some(first) = buf.chars().next() {
        if first == '\n' || first == '\r' || first == ' ' || first == '\t' {
            buf.drain(..first.len_utf8());
        } else {
            break;
        }
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
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_body: Option<Value>,
}

fn gemma4_extra_body(messages: &[ChatMessage]) -> Value {
    let enable_thinking = !messages.iter().any(is_fast_mode_message);
    serde_json::json!({
        "top_k": 64,
        "enable_thinking": enable_thinking,
        "chat_template_kwargs": {
            "enable_thinking": enable_thinking
        }
    })
}

fn is_fast_mode_message(message: &ChatMessage) -> bool {
    message.role == "system"
        && message
            .content
            .as_deref()
            .map(|content| {
                let lower = content.to_ascii_lowercase();
                lower.contains("mode_prompt: fast") || lower.contains("current mode: fast.")
            })
            .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::{
        AnswerStreamState, ContentStreamState, StreamEvent, clean_model_text, consume_sse_chunk,
        finish_content_stream, stream_answer_chunk, stream_content_chunk,
    };
    use tokio::sync::mpsc;

    #[test]
    fn clean_model_text_strips_gemma_thought_tokens() {
        let raw = "<|think|>reasoning<|/think|>final answer";
        assert_eq!(clean_model_text(raw), "final answer");
    }

    #[test]
    fn stream_content_chunk_handles_gemma_channel_thoughts_and_answer() {
        let mut state = ContentStreamState::default();
        let mut thought = String::new();
        let mut answer = String::new();

        for event in stream_content_chunk(&mut state, "<|channel>thought\nI will inspect") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }
        for event in stream_content_chunk(&mut state, "\n<channel|>The answer.") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }
        for event in finish_content_stream(&mut state) {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }

        assert_eq!(thought.trim_end(), "I will inspect");
        assert_eq!(answer, "The answer.");
    }

    #[test]
    fn stream_content_chunk_handles_channel_thought_without_newline() {
        let mut state = ContentStreamState::default();
        let mut thought = String::new();
        let mut answer = String::new();

        for event in stream_content_chunk(
            &mut state,
            "<|channel>thought I will inspect.<channel|>The answer.",
        ) {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }
        for event in finish_content_stream(&mut state) {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }

        assert_eq!(thought, "I will inspect.");
        assert_eq!(answer, "The answer.");
    }

    #[test]
    fn stream_content_chunk_handles_gemma_think_token_pair() {
        let mut state = ContentStreamState::default();
        let mut thought = String::new();
        let mut answer = String::new();

        for event in stream_content_chunk(&mut state, "<|think|>reason") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }
        for event in stream_content_chunk(&mut state, "ing<|/think|>final") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }
        for event in finish_content_stream(&mut state) {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }

        assert_eq!(thought, "reasoning");
        assert_eq!(answer, "final");
    }

    #[test]
    fn stream_content_chunk_preserves_non_gemma_thinking_tags_as_answer() {
        let mut state = ContentStreamState::default();
        let mut thought = String::new();
        let mut answer = String::new();

        for event in stream_content_chunk(&mut state, "<thinking>inspect repo") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }
        for event in stream_content_chunk(&mut state, "</thinking>Final.") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }
        for event in finish_content_stream(&mut state) {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }

        assert_eq!(thought, "");
        assert_eq!(answer, "<thinking>inspect repo</thinking>Final.");
    }

    #[test]
    fn stream_content_chunk_discards_secondary_thought_mid_answer() {
        let mut state = ContentStreamState::default();
        let mut thought = String::new();
        let mut answer = String::new();

        // Primary thought followed by partial answer.
        for event in stream_content_chunk(&mut state, "<|think|>first<|/think|>Keep re") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }

        // Secondary thought opens mid-word — body must be discarded, not sent to Zed.
        for event in stream_content_chunk(&mut state, "<|think|>second<|/think|>asoning") {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }

        for event in finish_content_stream(&mut state) {
            match event {
                StreamEvent::Thought(chunk) => thought.push_str(&chunk),
                StreamEvent::Answer(chunk) => answer.push_str(&chunk),
            }
        }

        // Only the primary thought is emitted; secondary is silently discarded.
        assert_eq!(thought, "first");
        assert_eq!(answer, "Keep reasoning");
    }

    #[test]
    fn stream_answer_chunk_sends_immediately() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnswerStreamState::default();

        stream_answer_chunk(&mut state, "Hello ", &tx);
        assert_eq!(rx.try_recv().unwrap(), "Hello ");

        stream_answer_chunk(&mut state, "world.", &tx);
        assert_eq!(rx.try_recv().unwrap(), "world.");
    }

    #[test]
    fn stream_answer_chunk_sanitizes_raw_combined_content() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnswerStreamState::default();

        stream_answer_chunk(
            &mut state,
            "<|channel>thought\nI am thinking.\n<channel|>Hello! How can I help?",
            &tx,
        );
        assert_eq!(rx.try_recv().unwrap(), "Hello! How can I help?");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stream_answer_chunk_long_answer_streams_immediately() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnswerStreamState::default();

        stream_answer_chunk(&mut state, &"a".repeat(600), &tx);
        // Sent immediately — no need to flush.
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn consume_sse_chunk_buffers_split_lines() {
        let mut pending = String::new();
        let mut lines = Vec::new();

        consume_sse_chunk(&mut pending, "data: one", |line| {
            lines.push(line.to_owned());
        });
        assert!(lines.is_empty());

        consume_sse_chunk(&mut pending, "\ndata: two\n", |line| {
            lines.push(line.to_owned());
        });

        assert_eq!(lines, vec!["data: one", "data: two"]);
    }

    #[test]
    fn gemma4_extra_body_disables_thinking_for_fast_mode_prompt() {
        let messages = vec![super::ChatMessage::system(
            "mode_prompt: fast\nCurrent mode: fast.",
        )];

        let body = super::gemma4_extra_body(&messages);
        assert_eq!(body["top_k"], 64);
        assert_eq!(body["enable_thinking"], false);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn gemma4_extra_body_enables_thinking_without_fast_mode_prompt() {
        let messages = vec![super::ChatMessage::system("Current mode: agent.")];

        let body = super::gemma4_extra_body(&messages);
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }
}
