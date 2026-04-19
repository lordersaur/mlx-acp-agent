use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::{Map, Value};

use crate::mlx_client::{
    ApiToolCall, ChatMessage, ChatToolCall, ChatToolCallFunction, CompletionResult, MlxClient,
};
use crate::model_parser::extract_thought_blocks;

pub const SYSTEM_PROMPT: &str = "\
You are a coding agent inside the user's editor. Use tools for all file and workspace operations — never guess.

Thinking:
- Think once, decide, act. Do not revisit a decision you already made.
- Never repeat the same reasoning in the think block. Each sentence must add new information.
- If you already know the answer from context (branch name, file path, tool list), use it — do not re-derive it.

Tools:
- Call tools immediately. Do not narrate what you are about to do — just call the tool.
- Call independent tools in parallel.
- Max 3 read_file_tool calls per response. Batch reads; continue next turn if more are needed.
- Never read the same file twice in one turn.
- If the task names a specific file, function, or symbol: your first action must be search_code_tool — never list_dir_tool. Use list_dir_tool only when you genuinely need to discover what files exist in an unknown directory.
- Search-anchor rule: use search_code_tool to locate a symbol. If the returned snippet answers the question, stop — do not open the file. If you need more context, read ONLY the relevant function: use the line number from the search result as start_line with a limit of 30-50 lines.
- BANNED: reading a file at start_line: 1 after search_code_tool already returned a line number for that file. BANNED: reading a file in sequential 100-line pages (start_line: 1, 101, 201…). Both are top-to-bottom paging and waste turns. If you catch yourself about to do either, stop and use search_code_tool instead.
- For explain-the-flow or trace-how-X-works tasks: search for specific function names (e.g. handle_session_prompt, run_agent_loop, persist_session), not broad keywords or module names. search_code_tool returns 3 lines of context around each match — if that is enough, answer directly. If not, read only that function using the returned line number as start_line.
- Never answer implementation questions from CLAUDE.md, README, or comments alone. If the question is about how code works, search the actual source and read the relevant function before answering.
- Use patch_file_tool for all targeted changes: adding lines, modifying values, appending code. Use edit_file_tool only when rewriting an entire file from scratch — keep the instruction one plain sentence, no quoted text inside it.
- Never announce that you are about to make a change and then stop. Call the tool immediately or say you cannot do it.
- Never claim to have made a change unless patch_file_tool or edit_file_tool returned successfully. If the last tool call was not one of those, no file was modified — do not say it was.
- If the user asks a yes/no question, the agent should answer it directly before explaining.
- Do not write meta labels like \"Self-Correction\", \"Refinement\", or similar process notes in thoughts or answers.

Files:
- Always use the exact file path returned by list_dir_tool or search_code_tool. Never construct a file path from memory — always get it from a tool first. If the user's message mentions a file path, verify it exists via search_code_tool before using it.
- list_dir_tool → answer from names only unless user asked what each file does.
- For counts, use a precise rg/grep/wc command via run_command_tool.

Commands:
- Long-running tasks (cargo build, cargo test, npm install): use start_command_session_tool, then poll with read_command_session_tool until `running: false` or `exit_code` appears.
- Never run build or test commands proactively. Exception: if you just modified source code, run cargo test once to verify the change compiles and tests pass.
- Always confirm the exit code of a run_command_tool call before reporting success.

Output:
- When a task is done (command exited 0, file written, etc.): state what was done in one sentence and stop. Do not speculate about next steps.
- If two different approaches both failed, stop and ask the user instead of trying a third.
- If the task is ambiguous (no specific file, function, or error cited), ask one clarifying question before using any tool. This applies even if you find something relevant during search — do not act on it without confirming it is the intended target.
- Ambiguous requests include phrases like \"something is slow\", \"make it smarter\", \"clean up the repo\", and \"fix the thing from last time\". Ask what the user means; do not search or edit.
- Never cite a specific line number in your answer unless you have read that line with read_file_tool.
- Use plain arrows like `->`; never use LaTeX arrows like `$\\rightarrow$`.
- If you can't do something, say so.";

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
            max_iterations: 15,
            max_tokens: 2500,
            temperature: 0.0,
            max_parallel_tool_calls: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ModelClient: Send + Sync {
    /// `think_tx`: when `Some`, the client streams `<think>` token chunks to the
    /// sender as they arrive (real-time thought animation). When `None`, falls back
    /// to the non-streaming path.
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        max_tokens: u32,
        temperature: f32,
        think_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
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
    ) -> Result<CompletionResult> {
        if let Some(tx) = think_tx {
            self.complete_streaming(messages, tools, max_tokens, temperature, tx)
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
        // When a thought handler is present, enable streaming so think tokens
        // arrive in real time (writing animation in the Zed panel).
        let (think_tx, mut think_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let maybe_tx = on_thought.is_some().then_some(think_tx);

        let result = {
            let complete_fut = model.complete(
                &conversation,
                tool_schemas,
                options.max_tokens,
                options.temperature,
                maybe_tx,
            );
            tokio::pin!(complete_fut);
            loop {
                tokio::select! {
                    res = &mut complete_fut => break res?,
                    Some(chunk) = think_rx.recv() => {
                        if let Some(ref mut handler) = on_thought {
                            handler.on_thought_chunk(&chunk).await;
                        }
                    }
                }
            }
        };

        // Drain any chunks that arrived just before complete() returned.
        let mut got_stream_chunks = false;
        while let Ok(chunk) = think_rx.try_recv() {
            got_stream_chunks = true;
            if let Some(ref mut handler) = on_thought {
                handler.on_thought_chunk(&chunk).await;
            }
        }
        let _ = got_stream_chunks;

        // Signal end-of-streaming-thought so Zed can flush a separator.
        if on_thought.is_some() {
            if let Some(ref mut handler) = on_thought {
                handler.on_thought_end().await;
            }
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

        if result.tool_calls.is_empty() {
            let answer = clean_text
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_default();
            let answer = prevent_unsupported_completion_claims(answer, &all_tool_results);

            return Ok(LoopResult {
                answer,
                tool_results: all_tool_results,
                iterations: iteration + 1,
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
        let tool_calls = if result.tool_calls.len() > options.max_parallel_tool_calls {
            &result.tool_calls[..options.max_parallel_tool_calls]
        } else {
            &result.tool_calls[..]
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

        // Execute tools (parallel when multiple).
        let executions = execute_tools(tool_calls, tools).await;
        all_tool_results.extend(executions.iter().cloned());

        // Push tool result messages with matching tool_call_id.
        for exec in &executions {
            let content = if exec.error {
                format!("Error: {}", exec.result)
            } else {
                exec.result.clone()
            };
            conversation.push(ChatMessage::tool_result(&exec.id, content));
        }
    }

    Ok(LoopResult {
        answer: "Reached maximum iterations.".to_owned(),
        tool_results: all_tool_results,
        iterations: options.max_iterations,
    })
}

// ---------------------------------------------------------------------------
// Qwen3.5 narration detection
// ---------------------------------------------------------------------------

/// Returns true when the model's response looks like mid-task narration rather
/// than a genuine final answer. Qwen3.5-9B occasionally produces text like
/// "cargo clean done, now I'll run cargo test" without calling the tool.
/// Claude/GPT-4o do not exhibit this behavior.
fn prevent_unsupported_completion_claims(answer: String, tool_results: &[ToolExecution]) -> String {
    let lower = answer.to_lowercase();
    let first_person_completion_claim = [
        "i created",
        "i added",
        "i updated",
        "i modified",
        "i edited",
        "i patched",
        "i deleted",
        "i wrote",
        "i implemented",
        "i applied",
        "i have created",
        "i have added",
        "i have updated",
        "i have modified",
        "i have edited",
        "i have patched",
        "i have deleted",
        "i have written",
        "i have implemented",
        "i have applied",
        "i've created",
        "i've added",
        "i've updated",
        "i've modified",
        "i've edited",
        "i've patched",
        "i've deleted",
        "i've written",
        "i've implemented",
        "i've applied",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
        || lower.starts_with("done")
        || lower.starts_with("fixed")
        || lower.starts_with("created")
        || lower.starts_with("added")
        || lower.starts_with("updated")
        || lower.starts_with("patched")
        || lower.starts_with("implemented");
    let has_successful_tool = |names: &[&str]| {
        tool_results
            .iter()
            .any(|tr| !tr.error && names.iter().any(|name| tr.name == *name))
    };
    let has_successful_command_result = || {
        tool_results.iter().any(|tr| {
            if tr.error {
                return false;
            }
            if !matches!(
                tr.name.as_str(),
                "run_command_tool" | "read_command_session_tool"
            ) {
                return false;
            }
            tr.result.contains("exit_code: 0")
                || tr.result.contains("\"exit_code\":0")
                || tr.result.contains("\"exit_code\": 0")
        })
    };

    let claims_file_change = [
        "created",
        "added",
        "updated",
        "modified",
        "edited",
        "patched",
        "deleted",
        "wrote",
        "implemented",
        "applied",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    let file_change_tools = [
        "create_artifact_tool",
        "edit_file_tool",
        "patch_file_tool",
        "delete_path_tool",
    ];
    if first_person_completion_claim
        && claims_file_change
        && !has_successful_tool(&file_change_tools)
    {
        return "I do not have a successful file-write tool result confirming that change."
            .to_owned();
    }

    let claims_tests_passed = lower.contains("test")
        && (lower.contains("passed") || lower.contains("pass") || lower.contains("cargo test"));
    if claims_tests_passed && !has_successful_command_result() {
        return "I do not have a successful command result confirming that tests passed."
            .to_owned();
    }

    answer
}

// ---------------------------------------------------------------------------
// Tool execution helpers
// ---------------------------------------------------------------------------

async fn execute_tools(tool_calls: &[ApiToolCall], tools: &dyn ToolExecutor) -> Vec<ToolExecution> {
    if tool_calls.len() == 1 {
        return vec![execute_one(&tool_calls[0], tools).await];
    }

    join_all(tool_calls.iter().map(|tc| execute_one(tc, tools))).await
}

async fn execute_one(tool_call: &ApiToolCall, tools: &dyn ToolExecutor) -> ToolExecution {
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

    struct MockTools {
        names: Vec<String>,
        outputs: HashMap<String, Result<String>>,
        calls: Arc<Mutex<Vec<(String, Map<String, Value>)>>>,
    }

    impl MockTools {
        fn with_outputs(outputs: HashMap<String, Result<String>>) -> Self {
            let names = outputs.keys().cloned().collect();
            Self {
                names,
                outputs,
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

            match self.outputs.get(name) {
                Some(Ok(result)) => Ok(result.clone()),
                Some(Err(error)) => Err(anyhow!(error.to_string())),
                None => Err(anyhow!("missing mock tool output")),
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
        assert!(second.iter().any(|m| m.role == "tool"));
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
    }

    #[tokio::test]
    async fn extracts_thoughts_from_content() {
        let model = MockModel::new(vec![text_response(
            "<thinking>I'll check the file.</thinking>The answer is 42.",
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
    async fn blocks_file_change_claims_without_write_tool_result() {
        let model = MockModel::new(vec![text_response(
            "I implemented the retry mechanism in src/agent_loop.rs.",
        )]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Make the agent smarter.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(
            result.answer,
            "I do not have a successful file-write tool result confirming that change."
        );
        assert!(result.tool_results.is_empty());
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
    async fn blocks_done_style_file_change_claims_without_write_tool_result() {
        let model = MockModel::new(vec![text_response("Updated README.md.")]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Update README.md.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(
            result.answer,
            "I do not have a successful file-write tool result confirming that change."
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
    async fn blocks_test_pass_claims_without_command_result() {
        let model = MockModel::new(vec![text_response("I ran cargo test, and tests passed.")]);
        let tools = MockTools::with_outputs(HashMap::new());

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Change the code.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(
            result.answer,
            "I do not have a successful command result confirming that tests passed."
        );
    }

    #[tokio::test]
    async fn blocks_test_pass_claims_after_nonzero_command_result() {
        let model = MockModel::new(vec![
            tool_call_response("call_1", "run_command_tool", json!({"cmd": "cargo test"})),
            text_response("I ran cargo test, and tests passed."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "run_command_tool".to_owned(),
            Ok("$ cargo test\n\nexit_code: 1\n\nstdout:\nfailed\n\nstderr:\n".to_owned()),
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

        assert_eq!(
            result.answer,
            "I do not have a successful command result confirming that tests passed."
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
            Ok("$ cargo test\n\nexit_code: 0\n\nstdout:\nok\n\nstderr:\n".to_owned()),
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
        assert!(SYSTEM_PROMPT.contains("Max 3 read_file_tool calls per response"));
        assert!(SYSTEM_PROMPT.contains("Never run build or test commands"));
        assert!(SYSTEM_PROMPT.contains("answer from names only"));
        assert!(SYSTEM_PROMPT.contains("search_code_tool"));
        assert!(SYSTEM_PROMPT.contains("one sentence"));
        assert!(SYSTEM_PROMPT.contains("start_command_session_tool"));
        assert!(SYSTEM_PROMPT.contains("read_command_session_tool"));
        assert!(SYSTEM_PROMPT.contains("something is slow"));
        assert!(SYSTEM_PROMPT.contains("fix the thing from last time"));
    }
}
