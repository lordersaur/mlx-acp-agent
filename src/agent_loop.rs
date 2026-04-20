use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::{Map, Value, json};

use crate::mlx_client::{
    ApiToolCall, ChatMessage, ChatToolCall, ChatToolCallFunction, CompletionResult, MlxClient,
};
use crate::model_parser::extract_thought_blocks;

pub const SYSTEM_PROMPT: &str = "\
You are a coding agent inside the user's editor.
Ground all claims in code evidence via tools.

Rules:
- Do not narrate or plan. Act immediately.
- Start with concrete symbols, dispatch entries, or store functions.
- Avoid semantic phrase searches. Use code tokens.
- One small grounded action per turn.
- Be concise. Cite exact file/function names in the final answer.
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
        while let Ok(chunk) = think_rx.try_recv() {
            if let Some(ref mut handler) = on_thought {
                handler.on_thought_chunk(&chunk).await;
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

        if result.tool_calls.is_empty() {
            let answer = clean_text
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_default();
            let answer = prevent_malformed_tool_call_answer(answer);

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

        // Execute tools (parallel when multiple), but avoid repeating the exact
        // same failing call indefinitely.
        let executions = execute_tools(tool_calls, tools, &all_tool_results).await;
        all_tool_results.extend(executions.iter().cloned());

        // Push tool result messages with matching tool_call_id.
        for exec in &executions {
            let content = model_tool_result_content(exec);
            conversation.push(ChatMessage::tool_result(&exec.id, content));
        }
    }

    Ok(LoopResult {
        answer: "Reached maximum iterations.".to_owned(),
        tool_results: all_tool_results,
        iterations: options.max_iterations,
    })
}

fn prevent_malformed_tool_call_answer(answer: String) -> String {
    if answer.contains("<|tool_call>")
        || answer.contains("<tool_call>")
        || answer.contains("<tool_call|>")
    {
        return "I tried to call a tool, but the tool call was malformed and could not be executed. No tool action was completed."
            .to_owned();
    }

    answer
        .replace("$\\rightarrow$", "->")
        .replace("$\\to$", "->")
}

// ---------------------------------------------------------------------------
// Tool execution helpers
// ---------------------------------------------------------------------------

fn model_tool_result_content(exec: &ToolExecution) -> String {
    let envelope = if exec.error {
        json!({
            "tool": exec.name,
            "status": "failed",
            "input": exec.arguments,
            "error": model_tool_error(&exec.result),
            "output": Value::Null,
        })
    } else {
        json!({
            "tool": exec.name,
            "status": "completed",
            "input": exec.arguments,
            "error": Value::Null,
            "output": exec.result,
        })
    };

    serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| exec.result.clone())
}

fn model_tool_error(result: &str) -> Value {
    if let Ok(Value::Object(mut structured)) = serde_json::from_str::<Value>(result) {
        if structured.contains_key("code")
            || structured.contains_key("message")
            || structured.contains_key("diagnostics")
        {
            let code = structured
                .remove("code")
                .unwrap_or_else(|| Value::String("tool_failed".to_owned()));
            let message = structured
                .remove("message")
                .unwrap_or_else(|| Value::String(result.to_owned()));
            let diagnostics = structured.remove("diagnostics");

            let mut error = Map::new();
            error.insert("code".to_owned(), code);
            error.insert("message".to_owned(), message);
            if let Some(diagnostics) = diagnostics {
                error.insert("diagnostics".to_owned(), diagnostics);
            }
            if !structured.is_empty() {
                error.insert("details".to_owned(), Value::Object(structured));
            }
            return Value::Object(error);
        }
    }

    json!({
        "code": "tool_failed",
        "message": result,
    })
}

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
            result: "Skipped repeated failed tool call with the same arguments. Re-read the relevant file/output and choose a different, smaller patch or another tool instead of retrying this call.".to_owned(),
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
        let tool_msg = second.iter().find(|m| m.role == "tool").expect("tool msg");
        let envelope: Value =
            serde_json::from_str(tool_msg.content.as_deref().unwrap()).expect("tool envelope");
        assert_eq!(envelope["tool"], "list_dir_tool");
        assert_eq!(envelope["status"], "completed");
        assert_eq!(envelope["input"]["path"], ".");
        assert_eq!(envelope["output"], "src\nCargo.toml");
        assert!(envelope["error"].is_null());
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
        let envelope: Value =
            serde_json::from_str(tool_msg.content.as_deref().unwrap()).expect("tool envelope");
        assert_eq!(envelope["tool"], "missing_tool");
        assert_eq!(envelope["status"], "failed");
        assert_eq!(envelope["input"]["path"], "agent.py");
        assert_eq!(envelope["error"]["code"], "tool_failed");
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Unknown tool")
        );
        assert!(envelope["output"].is_null());
    }

    #[test]
    fn structured_tool_errors_are_embedded_in_model_envelope() {
        let exec = ToolExecution {
            id: "call_1".to_owned(),
            name: "patch_file_tool".to_owned(),
            arguments: json!({"path": "notes.txt", "old_text": "missing"})
                .as_object()
                .cloned()
                .unwrap(),
            result: json!({
                "code": "patch_target_not_found",
                "message": "Patch target not found in file",
                "diagnostics": {
                    "path": "notes.txt",
                    "exact_occurrences": 0,
                }
            })
            .to_string(),
            error: true,
        };

        let envelope: Value =
            serde_json::from_str(&model_tool_result_content(&exec)).expect("tool envelope");
        assert_eq!(envelope["tool"], "patch_file_tool");
        assert_eq!(envelope["status"], "failed");
        assert_eq!(envelope["error"]["code"], "patch_target_not_found");
        assert_eq!(
            envelope["error"]["message"],
            "Patch target not found in file"
        );
        assert_eq!(envelope["error"]["diagnostics"]["exact_occurrences"], 0);
        assert!(envelope["output"].is_null());
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
            "Skipped repeated failed tool call with the same arguments. Re-read the relevant file/output and choose a different, smaller patch or another tool instead of retrying this call."
        );
        assert_eq!(tools.calls.lock().unwrap().len(), 2);
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
    async fn reports_malformed_tool_call_instead_of_raw_marker_text() {
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

        assert_eq!(
            result.answer,
            "I tried to call a tool, but the tool call was malformed and could not be executed. No tool action was completed."
        );
    }

    #[tokio::test]
    async fn reports_dangling_tool_call_terminator_as_malformed() {
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

        assert_eq!(
            result.answer,
            "I tried to call a tool, but the tool call was malformed and could not be executed. No tool action was completed."
        );
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
    async fn failed_command_result_is_sent_to_model_as_structured_envelope() {
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
        let envelope: Value =
            serde_json::from_str(tool_msg.content.as_deref().unwrap()).expect("tool envelope");
        assert_eq!(envelope["tool"], "run_command_tool");
        assert_eq!(envelope["status"], "completed");
        assert_eq!(envelope["input"]["cmd"], "cargo test");
        assert!(
            envelope["output"]
                .as_str()
                .unwrap()
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
        assert!(SYSTEM_PROMPT.contains("search_code_tool"));
        assert!(SYSTEM_PROMPT.contains("Did you mean"));
        assert!(SYSTEM_PROMPT.contains("concrete tokens from the user's requested boundary"));
        assert!(SYSTEM_PROMPT.contains("broad prose labels"));
        assert!(SYSTEM_PROMPT.contains("find_file_tool"));
        assert!(SYSTEM_PROMPT.contains("HEALTH_SANDBOX/fixture-crate"));
        assert!(SYSTEM_PROMPT.contains("one sentence"));
        assert!(SYSTEM_PROMPT.contains("start_command_session_tool"));
        assert!(SYSTEM_PROMPT.contains("read_command_session_tool"));
        assert!(SYSTEM_PROMPT.contains("clean up"));
        assert!(SYSTEM_PROMPT.contains("patch_file_tool"));
        assert!(SYSTEM_PROMPT.contains("exit code"));
        assert!(SYSTEM_PROMPT.contains("## Flow Tracing"));
        assert!(SYSTEM_PROMPT.contains("external entry point"));
        assert!(SYSTEM_PROMPT.contains("start boundary and the end boundary"));
        assert!(SYSTEM_PROMPT.contains("avoid hardcoding flow-specific function names"));
    }
}
