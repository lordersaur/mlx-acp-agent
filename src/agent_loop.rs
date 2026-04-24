use std::collections::BTreeSet;

use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::{Map, Value};

use crate::mlx_client::{
    ApiToolCall, ChatMessage, ChatToolCall, ChatToolCallFunction, CompletionResult, MlxClient,
};
use crate::model_parser::extract_thought_blocks;

pub const SYSTEM_PROMPT: &str = "\
You are a coding agent.
Use tools for source-backed claims.
Keep reasoning private.
Think efficiently and briefly.
Plan complex tasks internally before acting or answering.
Honor explicit constraints, delimiters, and examples.
For broad audits, use `list_dir_tool` with metadata to discover files, then `read_file_tool` for the relevant content. Continue truncated file reads until complete, and answer only after you have enough coverage. When you need full-file coverage, prefer parallel tool calls for independent files and use larger read limits instead of many tiny reads.
Treat returned source content as the material to analyze immediately; do not wait for more unless the tool explicitly says it is incomplete.
When coverage is complete, stop gathering and write the requested analysis or refactor plan immediately.
Do not restate the audit plan after coverage is complete.
When files are independent, read in parallel.
For file or module understanding, read before searching.
For narrow lookups, search first with `|`-separated alternates, then read the relevant lines.
Use line counts only to size reads.
Do not emit thinking tags, control tokens, or tool-call syntax in user-visible answers.
If unsure, say so.
";
// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A message from external callers (acp.rs history turns, system context).
/// Roles: "system", "user", "assistant".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub name: Option<String>,
}

impl ConversationMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            name: None,
        }
    }
}

/// A single completed tool invocation — recorded in session history.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolExecution {
    /// The `tool_call_id` from the model's tool call (used to correlate messages).
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
    pub result: String,
    pub error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoopResult {
    pub answer: String,
    pub tool_results: Vec<ToolExecution>,
    pub iterations: usize,
    pub answer_streamed: bool,
}

#[derive(Debug, Default)]
struct CoverageTracker {
    discovered_files: BTreeSet<String>,
    covered_files: BTreeSet<String>,
}

impl CoverageTracker {
    fn observe(&mut self, tool: &ToolExecution) {
        match tool.name.as_str() {
            "list_dir_tool" => self.observe_list_dir(&tool.arguments, &tool.result),
            "read_file_tool" => self.observe_read_file(&tool.arguments, &tool.result),
            _ => {}
        }
    }

    fn observe_list_dir(&mut self, arguments: &Map<String, Value>, result: &str) {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(result) else {
            return;
        };
        let Some(entries) = parsed.get("entries").and_then(Value::as_array) else {
            return;
        };
        for entry in entries {
            let Some(kind) = entry.get("kind").and_then(Value::as_str) else {
                continue;
            };
            if kind != "file" {
                continue;
            }
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            self.discovered_files
                .insert(normalize_relative_path(&resolve_child_path(path, name)));
        }
    }

    fn observe_read_file(&mut self, arguments: &Map<String, Value>, result: &str) {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return;
        };
        if !is_complete_file_chunk(result) {
            return;
        }
        self.covered_files.insert(normalize_relative_path(path));
    }

    fn is_complete(&self) -> bool {
        !self.discovered_files.is_empty()
            && self
                .discovered_files
                .iter()
                .all(|path| self.covered_files.contains(path))
    }

}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgentLoopOptions {
    pub max_iterations: usize,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Maximum number of tool calls executed per iteration. The model may
    /// request more; excess calls are silently dropped and picked up next turn.
    pub max_parallel_tool_calls: usize,
    /// Maximum characters kept per tool result before it is truncated.
    /// Prevents large file reads from filling GPU memory on the next inference.
    pub max_tool_result_chars: usize,
}

impl Default for AgentLoopOptions {
    fn default() -> Self {
        Self {
            max_iterations: 16,
            max_tokens: 3200,
            temperature: 0.6, // Qwen recommended for thinking mode
            max_parallel_tool_calls: 8,
            max_tool_result_chars: 12_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ModelClient: Send + Sync {
    /// `think_tx`: when `Some`, the client streams Gemma thinking token chunks to the
    /// sender as they arrive (real-time thought animation). When `None`, falls back
    /// to the non-streaming path.
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        max_tokens: u32,
        temperature: f32,
        think_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        answer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<CompletionResult>;
}

#[async_trait]
impl ModelClient for MlxClient {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        max_tokens: u32,
        temperature: f32,
        think_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        answer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<CompletionResult> {
        if let Some(tx) = think_tx {
            self.complete_streaming(messages, tools, max_tokens, temperature, tx, answer_tx)
                .await
        } else {
            self.complete_raw(messages, tools, max_tokens, temperature)
                .await
        }
    }
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn tool_names(&self) -> Vec<String>;

    async fn invoke(&self, name: &str, arguments: Map<String, Value>) -> Result<String>;

    fn has_tool(&self, name: &str) -> bool {
        self.tool_names().iter().any(|tool| tool == name)
    }
}

#[async_trait]
pub trait ThoughtHandler: Send {
    /// Complete thought (e.g. from non-streaming path). Implementations may
    /// add formatting (trailing newlines, separators).
    async fn on_thought(&mut self, thought: &str);

    /// Raw token chunk from the streaming path. No decoration — implementations
    /// should append the text verbatim and flush a separator only after the
    /// last chunk arrives (signalled by `on_thought_end`).
    async fn on_thought_chunk(&mut self, chunk: &str) {
        // Default: fall back to on_thought so test mocks work unchanged.
        self.on_thought(chunk).await;
    }

    /// Called once after all streaming chunks for a single thought have been sent.
    async fn on_thought_end(&mut self) {}

    /// Raw visible-answer chunk from the streaming path.
    async fn on_answer_chunk(&mut self, _chunk: &str) {}
}

// ---------------------------------------------------------------------------
// Agent loop
// ---------------------------------------------------------------------------

pub async fn run_agent_loop(
    model: &dyn ModelClient,
    messages: &[ConversationMessage],
    tools: &dyn ToolExecutor,
    tool_schemas: &[Value],
    mut on_thought: Option<&mut dyn ThoughtHandler>,
    options: AgentLoopOptions,
) -> Result<LoopResult> {
    let mut all_tool_results: Vec<ToolExecution> = Vec::new();
    let mut answer_streamed = false;
    let mut coverage = CoverageTracker::default();

    // Build the initial conversation as proper ChatMessages.
    let mut conversation: Vec<ChatMessage> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => conversation.push(ChatMessage::system(&msg.content)),
            "assistant" => conversation.push(ChatMessage::assistant(&msg.content)),
            _ => conversation.push(ChatMessage::user(&msg.content)),
        }
    }

    debug_assert!(conversation.iter().skip(1).all(|m| m.role != "system"));

    for iteration in 0..options.max_iterations {
        let mut iteration_reasoning = String::new();

        // When a thought handler is present, enable streaming so think tokens
        // arrive in real time (writing animation in the Zed panel).
        let (think_tx, mut think_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (answer_tx, mut answer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut answer_chunks: Vec<String> = Vec::new();
        let maybe_tx = on_thought.is_some().then_some(think_tx);
        let maybe_answer_tx = on_thought.is_some().then_some(answer_tx);

        let result = {
            let complete_fut = model.complete(
                &conversation,
                tool_schemas,
                options.max_tokens,
                options.temperature,
                maybe_tx,
                maybe_answer_tx,
            );
            tokio::pin!(complete_fut);
            loop {
                tokio::select! {
                    res = &mut complete_fut => break res?,
                    Some(chunk) = think_rx.recv() => {
                        if !chunk.is_empty() {
                            append_reasoning_chunk(&mut iteration_reasoning, &chunk);
                        }
                        if let Some(ref mut handler) = on_thought {
                            handler.on_thought_chunk(&chunk).await;
                        }
                    }
                    Some(chunk) = answer_rx.recv() => {
                        if !chunk.is_empty() {
                            answer_chunks.push(chunk);
                        }
                    }
                }
            }
        };

        // Drain any chunks that arrived just before complete() returned.
        while let Ok(chunk) = think_rx.try_recv() {
            if let Some(ref mut handler) = on_thought {
                handler.on_thought_chunk(&chunk).await;
            }
        }
        while let Ok(chunk) = answer_rx.try_recv() {
            if !chunk.is_empty() {
                answer_chunks.push(chunk);
            }
        }

        // Signal end-of-streaming-thought so Zed can flush a separator.
        if let Some(ref mut handler) = on_thought {
            handler.on_thought_end().await;
        }

        // Streaming path already stripped think tags from content; non-streaming
        // path returns raw text — extract thought blocks from it.
        let (thoughts, clean_text) = match result.content.as_deref() {
            Some(text) => {
                let (t, c) = extract_thought_blocks(text);
                (t, Some(c))
            }
            None => (Vec::new(), None),
        };

        // Only emit thoughts for the non-streaming path (streaming already sent them).
        if on_thought.is_some() && !thoughts.is_empty() {
            if let Some(handler) = on_thought.as_deref_mut() {
                for thought in &thoughts {
                    handler.on_thought(thought).await;
                }
            }
        }

        if !thoughts.is_empty() {
            append_reasoning_chunk(&mut iteration_reasoning, &thoughts.join("\n\n"));
        }

        if result.tool_calls.is_empty() {
            let answer = clean_text
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_default();

            if let Some(handler) = on_thought.as_deref_mut() {
                for chunk in &answer_chunks {
                    answer_streamed = true;
                    handler.on_answer_chunk(chunk).await;
                }
            }

            return Ok(LoopResult {
                answer,
                tool_results: all_tool_results,
                iterations: iteration + 1,
                answer_streamed,
            });
        }

        // Surface any pre-tool reasoning text as a thought.
        if let Some(handler) = on_thought.as_deref_mut() {
            if let Some(ref text) = clean_text {
                if !text.trim().is_empty() {
                    append_reasoning_chunk(&mut iteration_reasoning, text);
                    handler.on_thought(text).await;
                }
            }
        }

        // Cap parallel tool calls to avoid context overflow.
        // The model may request many calls at once; we silently truncate to
        // options.max_parallel_tool_calls so the conversation stays consistent —
        // the model will call the remaining tools in the next iteration.
        let tool_calls: Vec<ApiToolCall> =
            if result.tool_calls.len() > options.max_parallel_tool_calls {
                result.tool_calls[..options.max_parallel_tool_calls].to_vec()
            } else {
                result.tool_calls.clone()
            };

        // Push assistant message with structured tool_calls.
        let chat_tool_calls: Vec<ChatToolCall> = tool_calls
            .iter()
            .map(|tc| ChatToolCall {
                id: tc.id.clone(),
                kind: "function".to_owned(),
                function: ChatToolCallFunction {
                    name: tc.name.clone(),
                    arguments: serde_json::to_string(&tc.arguments)
                        .unwrap_or_else(|_| "{}".to_owned()),
                },
            })
            .collect();
        conversation.push(ChatMessage::assistant_with_tool_calls(chat_tool_calls));

        // Execute tools (parallel when multiple), but avoid repeating the exact
        // same failing call indefinitely.
        let executions = execute_tools(&tool_calls, tools, &all_tool_results).await;
        all_tool_results.extend(executions.iter().cloned());
        for exec in &executions {
            coverage.observe(exec);
        }

        // Push tool result messages with matching tool_call_id.
        // Truncate large results to avoid GPU OOM on the next inference.
        for exec in &executions {
            let result = truncate_tool_result(&exec.result, options.max_tool_result_chars);
            conversation.push(ChatMessage::tool_result(&exec.id, result));
        }

        if let Some(summary) =
            summarize_reasoning_for_context(&iteration_reasoning, coverage.is_complete())
        {
            conversation.push(ChatMessage::assistant(summary));
        }
    }

    Ok(LoopResult {
        answer: "Reached maximum iterations.".to_owned(),
        tool_results: all_tool_results,
        iterations: options.max_iterations,
        answer_streamed,
    })
}

// ---------------------------------------------------------------------------
// Tool execution helpers
// ---------------------------------------------------------------------------

async fn execute_tools(
    tool_calls: &[ApiToolCall],
    tools: &dyn ToolExecutor,
    previous_results: &[ToolExecution],
) -> Vec<ToolExecution> {
    if tool_calls.len() == 1 {
        return vec![execute_one(&tool_calls[0], tools, previous_results).await];
    }

    join_all(
        tool_calls
            .iter()
            .map(|tc| execute_one(tc, tools, previous_results)),
    )
    .await
}

async fn execute_one(
    tool_call: &ApiToolCall,
    tools: &dyn ToolExecutor,
    previous_results: &[ToolExecution],
) -> ToolExecution {
    if !tools.has_tool(&tool_call.name) {
        return ToolExecution {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            result: format!(
                "Unknown tool: {}. Available: {}",
                tool_call.name,
                tools.tool_names().join(", ")
            ),
            error: true,
        };
    }

    if repeated_failed_call_count(tool_call, previous_results) >= 2 {
        return ToolExecution {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            result: "Repeated failed tool call with the same arguments. Reuse the earlier failure signal, then choose a different tool or a smaller change instead of retrying this call.".to_owned(),
            error: true,
        };
    }

    if is_context_gathering_tool(&tool_call.name)
        && repeated_successful_call_count(tool_call, previous_results) >= 1
    {
        let replay = previous_results.iter().rev().find(|result| {
            !result.error && result.name == tool_call.name && result.arguments == tool_call.arguments
        });
        return ToolExecution {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            result: replay
                .map(|result| result.result.clone())
                .unwrap_or_else(|| {
                    "Reused earlier result for this exact context-gathering tool call. Continue from that result instead of re-reading the same content.".to_owned()
                }),
            error: false,
        };
    }

    match tools
        .invoke(&tool_call.name, tool_call.arguments.clone())
        .await
    {
        Ok(result) => ToolExecution {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            result,
            error: false,
        },
        Err(error) => ToolExecution {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            result: error.to_string(),
            error: true,
        },
    }
}

fn append_reasoning_chunk(buf: &mut String, chunk: &str) {
    let normalized = chunk
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(&normalized);
}

fn summarize_reasoning_for_context(reasoning: &str, coverage_complete: bool) -> Option<String> {
    let compact = reasoning
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned();
    if compact.is_empty() {
        return None;
    }

    let mut summary = to_past_tense_summary(&compact);
    if coverage_complete {
        summary.push_str(" Coverage had been completed.");
    }

    const MAX_SUMMARY_CHARS: usize = 360;
    if summary.len() <= MAX_SUMMARY_CHARS {
        return Some(summary);
    }

    let mut end = MAX_SUMMARY_CHARS;
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{} ...", &summary[..end]))
}

fn to_past_tense_summary(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches('.');
    let lower_first = |value: &str| {
        let mut chars = value.chars();
        match chars.next() {
            Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    };

    let past = if let Some(rest) = trimmed.strip_prefix("I should ") {
        format!("I had decided to {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("I will ") {
        format!("I had planned to {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("I need to ") {
        format!("I had needed to {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("I want to ") {
        format!("I had wanted to {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("I must ") {
        format!("I had to {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("Let's ") {
        format!("I had decided to {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("We should ") {
        format!("I had decided that we should {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("I am ") {
        format!("I had been {}", lower_first(rest))
    } else if let Some(rest) = trimmed.strip_prefix("I was ") {
        format!("I had been {}", lower_first(rest))
    } else {
        format!("I had already {}", lower_first(trimmed))
    };

    if past.ends_with('.') {
        past
    } else {
        format!("{past}.")
    }
}

fn resolve_child_path(parent: &str, child: &str) -> String {
    std::path::Path::new(parent)
        .join(child)
        .display()
        .to_string()
}

fn normalize_relative_path(path: &str) -> String {
    let normalized = path.trim().replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    normalized.trim_matches('/').to_owned()
}

fn is_complete_file_chunk(result: &str) -> bool {
    !result.contains("[Continue at line ")
        && !result.contains("[start_line ")
        && !result.contains("truncated")
}

fn repeated_failed_call_count(
    tool_call: &ApiToolCall,
    previous_results: &[ToolExecution],
) -> usize {
    previous_results
        .iter()
        .filter(|result| {
            result.error && result.name == tool_call.name && result.arguments == tool_call.arguments
        })
        .count()
}

fn repeated_successful_call_count(
    tool_call: &ApiToolCall,
    previous_results: &[ToolExecution],
) -> usize {
    previous_results
        .iter()
        .filter(|result| {
            !result.error && result.name == tool_call.name && result.arguments == tool_call.arguments
        })
        .count()
}

fn is_context_gathering_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file_tool"
            | "list_dir_tool"
            | "search_code_tool"
            | "find_file_tool"
            | "web_fetch_tool"
            | "web_search_tool"
    )
}

fn truncate_tool_result(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    // Snap to a UTF-8 char boundary so we don't slice mid-codepoint.
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = text.len() - end;
    format!("{}\n... [truncated {omitted} chars]", &text[..end])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;
    use serde_json::json;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::mlx_client::{ApiToolCall, ChatMessage, CompletionResult};

    // ------------------------------------------------------------------
    // Mock model
    // ------------------------------------------------------------------

    struct MockModel {
        responses: AsyncMutex<VecDeque<CompletionResult>>,
        requests: Arc<AsyncMutex<Vec<Vec<ChatMessage>>>>,
    }

    impl MockModel {
        fn new(responses: Vec<CompletionResult>) -> Self {
            Self {
                responses: AsyncMutex::new(responses.into_iter().collect()),
                requests: Arc::new(AsyncMutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl ModelClient for MockModel {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            _tools: &[Value],
            _max_tokens: u32,
            _temperature: f32,
            _think_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
            _answer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        ) -> Result<CompletionResult> {
            self.requests.lock().await.push(messages.to_vec());
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow!("no mock responses left"))
        }
    }

    // ------------------------------------------------------------------
    // Mock tools
    // ------------------------------------------------------------------

    #[derive(Clone)]
    enum MockToolOutput {
        Ok(String),
        Err(String),
    }

    struct MockTools {
        names: Vec<String>,
        outputs: Arc<Mutex<HashMap<String, VecDeque<MockToolOutput>>>>,
        calls: Arc<Mutex<Vec<(String, Map<String, Value>)>>>,
    }

    impl MockTools {
        fn with_outputs(outputs: HashMap<String, Result<String>>) -> Self {
            let names = outputs.keys().cloned().collect();
            let outputs = outputs
                .into_iter()
                .map(|(name, result)| {
                    let output = match result {
                        Ok(value) => MockToolOutput::Ok(value),
                        Err(error) => MockToolOutput::Err(error.to_string()),
                    };
                    (name, std::iter::once(output).collect())
                })
                .collect();
            Self {
                names,
                outputs: Arc::new(Mutex::new(outputs)),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl ToolExecutor for MockTools {
        fn tool_names(&self) -> Vec<String> {
            self.names.clone()
        }

        async fn invoke(&self, name: &str, arguments: Map<String, Value>) -> Result<String> {
            self.calls
                .lock()
                .expect("mock tool calls lock poisoned")
                .push((name.to_owned(), arguments));

            let mut outputs = self
                .outputs
                .lock()
                .expect("mock tool outputs lock poisoned");
            let Some(queue) = outputs.get_mut(name) else {
                return Err(anyhow!("missing mock tool output"));
            };
            let Some(next) = queue.pop_front() else {
                return Err(anyhow!("missing mock tool output"));
            };

            match next {
                MockToolOutput::Ok(result) => Ok(result),
                MockToolOutput::Err(error) => Err(anyhow!(error)),
            }
        }
    }

    // ------------------------------------------------------------------
    // Thought recorder
    // ------------------------------------------------------------------

    #[derive(Default)]
    struct RecordingThoughts {
        thoughts: Vec<String>,
    }

    #[async_trait]
    impl ThoughtHandler for RecordingThoughts {
        async fn on_thought(&mut self, thought: &str) {
            self.thoughts.push(thought.to_owned());
        }
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    fn text_response(content: &str) -> CompletionResult {
        CompletionResult {
            content: Some(content.to_owned()),
            tool_calls: Vec::new(),
        }
    }

    fn tool_call_response(id: &str, name: &str, args: Value) -> CompletionResult {
        CompletionResult {
            content: None,
            tool_calls: vec![ApiToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: args.as_object().cloned().unwrap_or_default(),
            }],
        }
    }

    #[tokio::test]
    async fn returns_text_response_directly() {
        let model = MockModel::new(vec![text_response("Hello!")]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Say hello.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Hello!");
        assert_eq!(result.iterations, 1);
        assert!(result.tool_results.is_empty());
    }

    #[tokio::test]
    async fn executes_tool_then_returns_answer() {
        let model = MockModel::new(vec![
            tool_call_response("call_1", "list_dir_tool", json!({"path": "."})),
            text_response("Found src and Cargo.toml."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "list_dir_tool".to_owned(),
            Ok("src\nCargo.toml".to_owned()),
        )]));
        let mut thoughts = RecordingThoughts::default();

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "List the repo.")],
            &tools,
            &[],
            Some(&mut thoughts),
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Found src and Cargo.toml.");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].id, "call_1");
        assert_eq!(result.tool_results[0].name, "list_dir_tool");
        assert_eq!(result.tool_results[0].result, "src\nCargo.toml");

        // Verify the second model request had the tool result message.
        let requests = model.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let second = &requests[1];
        // Should contain a "tool" role message with the result.
        let tool_msg = second.iter().find(|m| m.role == "tool").expect("tool msg");
        assert_eq!(tool_msg.content.as_deref(), Some("src\nCargo.toml"));
    }

    #[tokio::test]
    async fn records_unknown_tool_errors_without_failing_loop() {
        let model = MockModel::new(vec![
            tool_call_response("call_x", "missing_tool", json!({"path": "agent.py"})),
            text_response("Could not run it."),
        ]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Use a missing tool.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Could not run it.");
        assert_eq!(result.tool_results.len(), 1);
        assert!(result.tool_results[0].error);
        assert_eq!(
            result.tool_results[0].result,
            "Unknown tool: missing_tool. Available: "
        );

        let requests = model.requests.lock().await;
        let second = &requests[1];
        let tool_msg = second.iter().find(|m| m.role == "tool").expect("tool msg");
        let content = tool_msg.content.as_deref().unwrap_or_default();
        assert!(content.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn skips_exact_repeated_failed_tool_call_after_two_attempts() {
        let args = json!({"path": "notes.txt", "old_text": "missing", "new_text": "new"});
        let model = MockModel::new(vec![
            tool_call_response("call_1", "patch_file_tool", args.clone()),
            tool_call_response("call_2", "patch_file_tool", args.clone()),
            tool_call_response("call_3", "patch_file_tool", args),
            text_response("Could not patch it."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "patch_file_tool".to_owned(),
            Err(anyhow!("Patch target not found in file")),
        )]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Patch notes.txt.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Could not patch it.");
        assert_eq!(result.tool_results.len(), 3);
        assert_eq!(
            result.tool_results[2].result,
            "Repeated failed tool call with the same arguments. Reuse the earlier failure signal, then choose a different tool or a smaller change instead of retrying this call."
        );
        assert_eq!(tools.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn skips_exact_repeated_context_gathering_call_after_first_success() {
        let args = json!({"path": "src"});
        let model = MockModel::new(vec![
            tool_call_response("call_1", "list_dir_tool", args.clone()),
            tool_call_response("call_2", "list_dir_tool", args),
            text_response("I have enough context."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "list_dir_tool".to_owned(),
            Ok("acp.rs\nagent_loop.rs".to_owned()),
        )]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Inspect src.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "I have enough context.");
        assert_eq!(result.tool_results.len(), 2);
        assert_eq!(
            result.tool_results[1].result,
            "acp.rs\nagent_loop.rs"
        );
        assert_eq!(tools.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn injects_compact_reasoning_summary_before_next_iteration() {
        let model = MockModel::new(vec![
            CompletionResult {
                content: Some(
                    "<|channel>thought\nI should inspect the tree before planning.<channel|>"
                        .to_owned(),
                ),
                tool_calls: vec![ApiToolCall {
                    id: "call_1".to_owned(),
                    name: "list_dir_tool".to_owned(),
                    arguments: json!({"path": "src"}).as_object().cloned().unwrap(),
                }],
            },
            text_response("Done."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "list_dir_tool".to_owned(),
            Ok("acp.rs\nagent_loop.rs".to_owned()),
        )]));

        run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Inspect src.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        let requests = model.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let second = &requests[1];
        let summary_msg = second
            .iter()
            .find(|msg| msg.role == "assistant" && msg.content.as_deref().unwrap_or_default().contains("I had decided"))
            .expect("reasoning summary");
        assert!(
            summary_msg
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("I had decided to inspect the tree before planning.")
        );
    }

    #[tokio::test]
    async fn extracts_thoughts_from_content() {
        let model = MockModel::new(vec![text_response(
            "<|channel>thought\nI'll check the file.<channel|>The answer is 42.",
        )]);
        let tools = MockTools::with_outputs(HashMap::new());
        let mut thoughts = RecordingThoughts::default();

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "What's the answer?")],
            &tools,
            &[],
            Some(&mut thoughts),
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "The answer is 42.");
        assert_eq!(thoughts.thoughts, vec!["I'll check the file."]);
    }

    #[tokio::test]
    async fn tool_call_ids_are_correlated_in_messages() {
        let model = MockModel::new(vec![
            tool_call_response("abc123", "list_dir_tool", json!({"path": "."})),
            text_response("Done."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "list_dir_tool".to_owned(),
            Ok("src".to_owned()),
        )]));

        run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "List.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        let requests = model.requests.lock().await;
        let second = &requests[1];
        // The tool result message should have tool_call_id == "abc123".
        let tool_msg = second.iter().find(|m| m.role == "tool").expect("tool msg");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn allows_explanations_that_mention_code_changes_without_claiming_action() {
        let model = MockModel::new(vec![text_response(
            "The flow updates SessionState, records commands, and persists the response.",
        )]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new(
                "user",
                "Explain the current ACP message flow.",
            )],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(
            result.answer,
            "The flow updates SessionState, records commands, and persists the response."
        );
    }

    #[tokio::test]
    async fn allows_file_change_claims_after_write_tool_result() {
        let model = MockModel::new(vec![
            tool_call_response(
                "call_1",
                "patch_file_tool",
                json!({"path": "README.md", "old_text": "old", "new_text": "new"}),
            ),
            text_response("Patched README.md."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "patch_file_tool".to_owned(),
            Ok("{\"status\":\"patched\"}".to_owned()),
        )]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Patch README.md.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Patched README.md.");
        assert_eq!(result.tool_results.len(), 1);
    }

    #[tokio::test]
    async fn preserves_malformed_tool_call_text_when_no_calls_are_parsed() {
        let model = MockModel::new(vec![text_response(
            r#"<|tool_call>call:create_artifact_tool{filename:"HEALTH_CONTRIBUTING.md",instruction:## Contributing

Run `cargo test`.<tool_call|>"#,
        )]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new(
                "user",
                "Create the contributing file.",
            )],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert!(result.answer.contains("create_artifact_tool"));
    }

    #[tokio::test]
    async fn preserves_dangling_tool_call_terminator_text() {
        let model = MockModel::new(vec![text_response(
            r#"call:patch_file_tool{path:"README.md"}<tool_call|>"#,
        )]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Patch README.md.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert!(result.answer.contains("patch_file_tool"));
    }

    #[tokio::test]
    async fn preserves_bare_gemma_tool_call_text() {
        let model = MockModel::new(vec![text_response(
            r#"read_file_tool{path:<|"|>src/agent_loop.rs<|"|>}"#,
        )]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Read agent_loop.rs.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert!(result.answer.contains("read_file_tool"));
    }

    #[tokio::test]
    async fn allows_documentation_that_mentions_cargo_test_without_command_result() {
        let model = MockModel::new(vec![text_response(
            "Build with `cargo build` and run tests with `cargo test`.",
        )]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new(
                "user",
                "Draft contributing instructions.",
            )],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(
            result.answer,
            "Build with `cargo build` and run tests with `cargo test`."
        );
    }

    #[tokio::test]
    async fn failed_command_result_is_sent_to_model_as_raw_tool_output() {
        let model = MockModel::new(vec![
            tool_call_response("call_1", "run_command_tool", json!({"cmd": "cargo test"})),
            text_response("Tests failed."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "run_command_tool".to_owned(),
            Ok(
                "$ cargo test\n\nexit_code: 1\n\noutput (stdout+stderr merged by PTY):\nfailed"
                    .to_owned(),
            ),
        )]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Run tests.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Tests failed.");

        let requests = model.requests.lock().await;
        let second = &requests[1];
        let tool_msg = second.iter().find(|m| m.role == "tool").expect("tool msg");
        assert!(
            tool_msg
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("exit_code: 1")
        );
    }

    #[tokio::test]
    async fn allows_test_pass_claims_after_zero_command_result() {
        let model = MockModel::new(vec![
            tool_call_response("call_1", "run_command_tool", json!({"cmd": "cargo test"})),
            text_response("I ran cargo test, and tests passed."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "run_command_tool".to_owned(),
            Ok(
                "$ cargo test\n\nexit_code: 0\n\noutput (stdout+stderr merged by PTY):\nok"
                    .to_owned(),
            ),
        )]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Run tests.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "I ran cargo test, and tests passed.");
    }

    #[test]
    fn truncate_tool_result_under_limit() {
        assert_eq!(truncate_tool_result("hello", 10), "hello");
    }

    #[test]
    fn truncate_tool_result_over_limit() {
        let long = "a".repeat(20_000);
        let result = truncate_tool_result(&long, 12_000);
        assert!(result.starts_with(&"a".repeat(12_000)));
        assert!(result.contains("[truncated 8000 chars]"));
    }

    #[test]
    fn truncate_tool_result_utf8_boundary() {
        // 3-byte UTF-8 codepoint — slicing mid-codepoint must not panic.
        let s = "€".repeat(10_000); // each '€' is 3 bytes = 30_000 bytes total
        let result = truncate_tool_result(&s, 10_001); // limit falls mid-codepoint
        // Must not panic, must be valid UTF-8, must be shorter than the original.
        assert!(result.len() < s.len());
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(result.contains("[truncated"));
    }

    #[test]
    fn system_prompt_has_key_rules() {
        assert!(SYSTEM_PROMPT.contains("You are a coding agent"));
        assert!(SYSTEM_PROMPT.contains("Use tools for source-backed claims"));
        assert!(SYSTEM_PROMPT.contains("Keep reasoning private"));
        assert!(SYSTEM_PROMPT.contains("Think efficiently and briefly"));
        assert!(SYSTEM_PROMPT.contains("Plan complex tasks internally"));
        assert!(SYSTEM_PROMPT.contains("constraints, delimiters, and examples"));
        assert!(SYSTEM_PROMPT.contains("For broad audits"));
        assert!(SYSTEM_PROMPT.contains("`list_dir_tool` with metadata to discover files"));
        assert!(SYSTEM_PROMPT.contains("Continue truncated file reads until complete"));
        assert!(SYSTEM_PROMPT.contains("When coverage is complete"));
        assert!(SYSTEM_PROMPT.contains("Do not restate the audit plan after coverage is complete"));
        assert!(SYSTEM_PROMPT.contains("When files are independent, read in parallel"));
        assert!(SYSTEM_PROMPT.contains("For file or module understanding"));
        assert!(SYSTEM_PROMPT.contains("search first with `|`-separated alternates"));
        assert!(SYSTEM_PROMPT.contains("Use line counts only to size reads"));
        assert!(SYSTEM_PROMPT.contains("Do not emit thinking tags"));
        assert!(!SYSTEM_PROMPT.contains("Gemma"));
    }
}
