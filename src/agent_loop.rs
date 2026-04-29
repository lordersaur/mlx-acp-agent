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
You are a coding agent called Gemma 4.
Think briefly in action-focused bullets before acting. Do not restate the user's request.
Honor the user's intent boundary. If asked to research, compare, recommend, or choose an approach, answer with the recommendation and ask before creating files, installing dependencies, or scaffolding. Build only after the user explicitly asks to proceed or has already chosen the stack.
Ask a concise clarification only when ambiguity blocks safe progress. Otherwise make reasonable assumptions and continue.
If the user says to continue, proceed, do it, ok, or stop asking for confirmation, keep working until blocked by missing information, unavailable tools, or validation failure.
Use tools for real inspection and changes. Never claim a file, command, or test changed unless a tool result shows it.
Before installing a missing runtime or package manager dependency, inspect the environment first: OS, available package managers, and whether the command needs interactive credentials. Do not try sudo unless the user explicitly requested sudo.
When a runtime, command, dependency, or stack is unavailable, stop that stack plan and ask whether to install the missing capability or switch paths. Do not invent files around a failed scaffold.
Use run_command_tool for finite commands, even if slow: build, test, install, format, scaffold. Use start_command_session_tool only for commands meant to stay alive, interactive commands, watchers, or later polling: dev servers, `dotnet run`, `npm run dev`.
For project commands, pass `cwd` instead of relying on a previous `cd`. Each command starts fresh.
File tools (read_file_tool, create_artifact_tool, edit_file_tool, patch_file_tool, list_dir_tool) always resolve paths from the workspace root. The `cwd` on a command tool is scoped to that single command and does not shift the base for file tools. Never assume you are inside a subdirectory — always use full workspace-relative paths (e.g. `MyApp/src/Page.tsx`, not `src/Page.tsx`).
Use parallel tool calls only for independent non-command work such as reads, searches, or creating unrelated files. Do not run package installs, builds, tests, dev servers, or other side-effecting commands in parallel.
For named files, read them directly. For named symbols, search first for the line, then read the relevant file section. Search snippets are candidates, not enough context by themselves.
After scaffolding, inspect the actual generated directories before assuming framework paths. If an expected path is missing, list nearby directories and follow the discovered structure.
Before any command that moves, copies, or restructures paths (mv, cp -r, rsync, rename, etc.), check whether the destination already exists using list_dir_tool. If the destination is an existing directory, shell commands like mv and cp will place the source inside it rather than replacing it — often producing unwanted nesting. Verify the target state first, then move contents explicitly if needed.
Before changing a public function signature or name, find call sites and update them in the same task.
Write or edit files with tools; do not draft file contents in assistant text. Never use create_artifact_tool for directories.
When a planned action requires a tool call, issue the tool call immediately — do not narrate it with phrases like 'I will...', 'First, I will...', or 'Action:' and then stop. A description without a following tool call is a failure.
Trust structured tool result fields first, especially `status`, `error`, `diagnostics`, `exit_code`, `running`, and `cwd`.
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
}

impl Default for AgentLoopOptions {
    fn default() -> Self {
        Self {
            max_iterations: 32,
            max_tokens: 12000,
            temperature: 0.3,
            // Gemma 4 emits at most 3 tool calls per turn (enforced by Python server).
            // Keep Rust in sync so the truncation logic here is never a surprise.
            max_parallel_tool_calls: 3,
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
        let mut thoughts_streamed = false;

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
                            thoughts_streamed = true;
                        }
                        if let Some(ref mut handler) = on_thought {
                            handler.on_thought_chunk(&chunk).await;
                        }
                    }
                    Some(chunk) = answer_rx.recv() => {
                        if !chunk.is_empty() {
                            answer_chunks.push(chunk.clone());
                            if let Some(ref mut handler) = on_thought {
                                answer_streamed = true;
                                handler.on_answer_chunk(&chunk).await;
                            }
                        }
                    }
                }
            }
        };

        // Drain any chunks that arrived just before complete() returned.
        while let Ok(chunk) = think_rx.try_recv() {
            if !chunk.is_empty() {
                thoughts_streamed = true;
            }
            if let Some(ref mut handler) = on_thought {
                handler.on_thought_chunk(&chunk).await;
            }
        }
        while let Ok(chunk) = answer_rx.try_recv() {
            if !chunk.is_empty() {
                answer_chunks.push(chunk.clone());
                if let Some(ref mut handler) = on_thought {
                    answer_streamed = true;
                    handler.on_answer_chunk(&chunk).await;
                }
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
        if !thoughts_streamed && on_thought.is_some() && !thoughts.is_empty() {
            if let Some(handler) = on_thought.as_deref_mut() {
                for thought in &thoughts {
                    handler.on_thought(thought).await;
                }
            }
        }

        if result.tool_calls.is_empty() {
            let answer = clean_text
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_default();

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
        // Cap individual results so a single oversized read cannot flood the context.
        const MAX_RESULT_CHARS: usize = 200_000;
        for exec in &executions {
            let result = if exec.result.len() > MAX_RESULT_CHARS {
                format!(
                    "{}\n[Result truncated: {} chars total, showing first {}. Read smaller chunks.]",
                    &exec.result[..MAX_RESULT_CHARS],
                    exec.result.len(),
                    MAX_RESULT_CHARS,
                )
            } else {
                exec.result.clone()
            };
            conversation.push(ChatMessage::tool_result(&exec.id, result));
        }

        let _ = coverage.is_complete();
    }

    Ok(LoopResult {
        answer: build_interruption_summary(&all_tool_results),
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
            !result.error
                && result.name == tool_call.name
                && result.arguments == tool_call.arguments
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
            !result.error
                && result.name == tool_call.name
                && result.arguments == tool_call.arguments
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

// ---------------------------------------------------------------------------
// Interruption summary
// ---------------------------------------------------------------------------

/// Builds a compact resume note when the loop hits max_iterations.
/// Derived purely from tool_results — no extra model call needed.
/// Stored as the final assistant turn so the next "continue" session has context.
fn build_interruption_summary(tool_results: &[ToolExecution]) -> String {
    let write_tools = ["create_artifact_tool", "edit_file_tool", "patch_file_tool"];
    let mut files_written: Vec<&str> = tool_results
        .iter()
        .filter(|t| write_tools.contains(&t.name.as_str()))
        .filter_map(|t| {
            t.arguments
                .get("path")
                .or_else(|| t.arguments.get("filename"))
                .and_then(Value::as_str)
        })
        .collect();
    files_written.dedup();

    let last_tool = tool_results
        .last()
        .map(|t| t.name.as_str())
        .unwrap_or("none");

    let files_section = if files_written.is_empty() {
        "  (none)".to_owned()
    } else {
        files_written
            .iter()
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "Interrupted: reached the iteration limit mid-task.\n\
        \n\
        Files written this session:\n\
        {files_section}\n\
        \n\
        Last action: {last_tool}\n\
        \n\
        Context for the next message:\n\
        - If asked to continue: start with list_dir_tool on the workspace root to confirm what exists, then resume from where it stopped without recreating existing files.\n\
        - If asked to correct or adjust something: apply the correction and continue.\n\
        - If asked to start over or do something new: ignore this summary and proceed with the new request."
    )
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
        assert_eq!(result.tool_results[1].result, "acp.rs\nagent_loop.rs");
        assert_eq!(tools.calls.lock().unwrap().len(), 1);
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
    async fn does_not_inject_reasoning_summary_before_next_iteration() {
        let model = MockModel::new(vec![
            tool_call_response("call_1", "list_dir_tool", json!({"path": "src"})),
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
        let second = &requests[1];
        assert!(!second.iter().any(|msg| {
            msg.role == "assistant" && msg.content.as_deref().unwrap_or_default().contains("I had")
        }));
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
    fn system_prompt_has_key_rules() {
        assert!(SYSTEM_PROMPT.contains("You are a coding agent called Gemma 4"));
        assert!(SYSTEM_PROMPT.contains("Do not restate the user's request"));
        assert!(SYSTEM_PROMPT.contains("Honor the user's intent boundary"));
        assert!(
            SYSTEM_PROMPT
                .contains("ask before creating files, installing dependencies, or scaffolding")
        );
        assert!(SYSTEM_PROMPT.contains("stop asking for confirmation"));
        assert!(SYSTEM_PROMPT.contains("inspect the environment first"));
        assert!(
            SYSTEM_PROMPT.contains("Do not try sudo unless the user explicitly requested sudo")
        );
        assert!(SYSTEM_PROMPT.contains("runtime, command, dependency, or stack is unavailable"));
        assert!(SYSTEM_PROMPT.contains("Use run_command_tool for finite commands"));
        assert!(
            SYSTEM_PROMPT
                .contains("Use start_command_session_tool only for commands meant to stay alive")
        );
        assert!(SYSTEM_PROMPT.contains("pass `cwd`"));
        assert!(SYSTEM_PROMPT.contains("Do not run package installs, builds, tests, dev servers"));
        assert!(SYSTEM_PROMPT.contains("Search snippets are candidates"));
        assert!(SYSTEM_PROMPT.contains("inspect the actual generated directories"));
        assert!(SYSTEM_PROMPT.contains("Never use create_artifact_tool for directories"));
    }
}
