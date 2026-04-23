use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::mlx_client::{
    ApiToolCall, ChatMessage, ChatToolCall, ChatToolCallFunction, CompletionResult, MlxClient,
};
use crate::model_parser::extract_thought_blocks;

pub const SYSTEM_PROMPT: &str = "\
You are a coding agent.
Use tools for code-grounded claims.
Search snippets are not source-read evidence.
Keep internal reasoning private.
Do not write Gemma control tokens, ACP thinking tags, or prose tool-call displays in user-visible answers.
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
            temperature: 1.0,
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
    let turn_contract = generate_turn_contract(model, messages, tools.tool_names()).await;

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

        if result.tool_calls.is_empty() {
            let answer = clean_text
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_default();

            let evidence = EvidenceLedger::from_tool_results(&all_tool_results);
            if let Some(instruction) = turn_contract.unmet_final_answer_instruction(&evidence) {
                conversation.push(ChatMessage::user(instruction));
                continue;
            }

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
            conversation.push(ChatMessage::tool_result(&exec.id, exec.result.clone()));
        }
    }

    Ok(LoopResult {
        answer: "Reached maximum iterations.".to_owned(),
        tool_results: all_tool_results,
        iterations: options.max_iterations,
        answer_streamed,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnContract {
    goal: String,
    allowed_to_answer_without_tools: bool,
    final_conditions: Vec<FinalCondition>,
    failure_policy: String,
}

impl TurnContract {
    fn fallback(messages: &[ConversationMessage]) -> Self {
        Self {
            goal: latest_user_task(messages),
            allowed_to_answer_without_tools: true,
            final_conditions: Vec::new(),
            failure_policy: "If required evidence is missing, say what is missing.".to_owned(),
        }
    }

    fn unmet_final_answer_instruction(&self, evidence: &EvidenceLedger) -> Option<String> {
        if self.allowed_to_answer_without_tools && self.final_conditions.is_empty() {
            return None;
        }

        let unmet: Vec<String> = self
            .final_conditions
            .iter()
            .filter(|condition| !evidence.satisfies(condition))
            .map(FinalCondition::retry_label)
            .collect();

        if unmet.is_empty() {
            None
        } else {
            Some(format!(
                "The current task has unmet final-answer conditions. Goal: {}. Missing evidence: {}. Continue with the needed tool calls, then answer only from verified tool evidence. Failure policy: {}",
                self.goal,
                unmet.join("; "),
                self.failure_policy
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FinalCondition {
    SourceRead { path_hint: Option<String> },
    WebSearch { query_hint: Option<String> },
    Write { path_hint: Option<String> },
    Command { command_hint: Option<String> },
}

impl FinalCondition {
    fn retry_label(&self) -> String {
        match self {
            FinalCondition::SourceRead { path_hint } => match path_hint {
                Some(path) => format!("successful read_file_tool evidence for `{path}`"),
                None => "successful read_file_tool evidence".to_owned(),
            },
            FinalCondition::WebSearch { query_hint } => match query_hint {
                Some(query) => format!("successful web_search_tool evidence for `{query}`"),
                None => "successful web_search_tool evidence".to_owned(),
            },
            FinalCondition::Write { path_hint } => match path_hint {
                Some(path) => format!("successful write-tool evidence for `{path}`"),
                None => "successful write-tool evidence".to_owned(),
            },
            FinalCondition::Command { command_hint } => match command_hint {
                Some(command) => format!("successful run_command_tool evidence for `{command}`"),
                None => "successful run_command_tool evidence".to_owned(),
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EvidenceLedger {
    successful_read_paths: Vec<String>,
    successful_web_queries: Vec<String>,
    successful_write_paths: Vec<String>,
    successful_commands: Vec<String>,
}

impl EvidenceLedger {
    fn from_tool_results(tool_results: &[ToolExecution]) -> Self {
        let mut ledger = Self::default();

        for result in tool_results.iter().filter(|result| !result.error) {
            match result.name.as_str() {
                "read_file_tool" => {
                    if let Some(path) = string_arg(&result.arguments, "path") {
                        if !path.is_empty() {
                            ledger.successful_read_paths.push(normalize_path_hint(path));
                        }
                    }
                }
                "web_search_tool" => {
                    if let Some(query) = string_arg(&result.arguments, "query") {
                        if !query.is_empty() {
                            ledger
                                .successful_web_queries
                                .push(normalize_text_hint(query));
                        }
                    }
                }
                "web_fetch_tool" => {
                    if let Some(url) = string_arg(&result.arguments, "url") {
                        if !url.is_empty() {
                            ledger.successful_web_queries.push(normalize_text_hint(url));
                        }
                    }
                }
                "create_artifact_tool"
                | "edit_file_tool"
                | "patch_file_tool"
                | "delete_path_tool" => {
                    if let Some(path) = write_path_from_execution(result) {
                        ledger
                            .successful_write_paths
                            .push(normalize_path_hint(&path));
                    }
                }
                "run_command_tool" | "start_command_session_tool" => {
                    if let Some(cmd) = string_arg(&result.arguments, "cmd") {
                        if !cmd.is_empty() {
                            ledger.successful_commands.push(normalize_text_hint(cmd));
                        }
                    }
                }
                _ => {}
            }
        }

        ledger
    }

    fn satisfies(&self, condition: &FinalCondition) -> bool {
        match condition {
            FinalCondition::SourceRead { path_hint } => match path_hint {
                Some(hint) => self.has_read_matching(hint),
                None => !self.successful_read_paths.is_empty(),
            },
            FinalCondition::WebSearch { query_hint } => match query_hint {
                Some(hint) => self.has_web_matching(hint),
                None => !self.successful_web_queries.is_empty(),
            },
            FinalCondition::Write { path_hint } => match path_hint {
                Some(hint) => self.has_write_matching(hint),
                None => !self.successful_write_paths.is_empty(),
            },
            FinalCondition::Command { command_hint } => match command_hint {
                Some(hint) => self.has_command_matching(hint),
                None => !self.successful_commands.is_empty(),
            },
        }
    }

    fn has_read_matching(&self, path_hint: &str) -> bool {
        let hint = normalize_path_hint(path_hint);
        self.successful_read_paths.iter().any(|path| {
            path == &hint || path.ends_with(&format!("/{hint}")) || hint.ends_with(path)
        })
    }

    fn has_write_matching(&self, path_hint: &str) -> bool {
        let hint = normalize_path_hint(path_hint);
        self.successful_write_paths.iter().any(|path| {
            path == &hint || path.ends_with(&format!("/{hint}")) || hint.ends_with(path)
        })
    }

    fn has_web_matching(&self, query_hint: &str) -> bool {
        let hint = normalize_text_hint(query_hint);
        self.successful_web_queries
            .iter()
            .any(|query| query.contains(&hint) || hint.contains(query))
    }

    fn has_command_matching(&self, command_hint: &str) -> bool {
        let hint = normalize_text_hint(command_hint);
        self.successful_commands
            .iter()
            .any(|cmd| cmd.contains(&hint) || hint.contains(cmd))
    }
}

#[derive(Debug, Deserialize)]
struct ContractEnvelope {
    goal: Option<String>,
    allowed_to_answer_without_tools: Option<bool>,
    final_conditions: Option<Vec<ContractCondition>>,
    failure_policy: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContractCondition {
    #[serde(rename = "type")]
    kind: String,
    path_hint: Option<String>,
    query_hint: Option<String>,
    command_hint: Option<String>,
}

async fn generate_turn_contract(
    model: &dyn ModelClient,
    messages: &[ConversationMessage],
    tool_names: Vec<String>,
) -> TurnContract {
    let task = latest_user_task(messages);
    let recent_context = recent_contract_context(messages);
    let prompt = format!(
        "Current user task:\n{task}\n\nRecent conversation context:\n{recent_context}\n\nAvailable tools:\n{}",
        tool_names.join(", ")
    );
    let contract_messages = [
        ChatMessage::system(
            "mode_prompt: fast\nTask contract classifier for a local coding agent. Return raw JSON only, with no Markdown fences and no explanation. Schema: {\"goal\":string,\"allowed_to_answer_without_tools\":boolean,\"final_conditions\":[{\"type\":\"source_read\"|\"web_search\"|\"write\"|\"command\"|\"none\",\"path_hint\":string|null,\"query_hint\":string|null,\"command_hint\":string|null,\"reason\":string}],\"failure_policy\":string}.\n\nChoose final conditions needed before the agent may give a final answer. Prior conversation matters: references like \"this\", \"that\", \"the one we discussed\", \"the plan\", and \"this agent\" inherit the recent topic.\n\nRules:\n- Treat filenames, paths, extensions, symbols, code behavior, and repository/project-specific questions as local workspace tasks first.\n- For local code/file behavior questions, project-specific architecture plans, or critiques of prior codebase analysis, require source_read and set path_hint to the named file/path/symbol when possible.\n- Search snippets are not source-read evidence; source_read means read_file_tool evidence.\n- If the user explicitly asks to search the internet/web, or asks for current/latest external information, require web_search and set query_hint when possible.\n- If the user asks to modify/create/delete files, require write and set path_hint when possible.\n- If the user asks to run tests, builds, diagnostics, commands, or verification, require command and set command_hint when possible.\n- Use none only for casual chat or genuinely general advice not tied to the local project or recent repo-specific discussion.",
        ),
        ChatMessage::user(prompt),
    ];

    let Ok(result) = model
        .complete(&contract_messages, &[], 450, 0.0, None, None)
        .await
    else {
        return TurnContract::fallback(messages);
    };

    let Some(content) = result.content else {
        return TurnContract::fallback(messages);
    };

    parse_turn_contract(&content).unwrap_or_else(|| TurnContract::fallback(messages))
}

fn parse_turn_contract(content: &str) -> Option<TurnContract> {
    let (_thoughts, clean) = extract_thought_blocks(content);
    let json_text = extract_json_object(&clean)?;
    let parsed: ContractEnvelope = serde_json::from_str(json_text).ok()?;
    let final_conditions = parsed
        .final_conditions
        .unwrap_or_default()
        .into_iter()
        .filter_map(|condition| match condition.kind.as_str() {
            "source_read" => Some(FinalCondition::SourceRead {
                path_hint: condition.path_hint.filter(|s| !s.trim().is_empty()),
            }),
            "web_search" => Some(FinalCondition::WebSearch {
                query_hint: condition.query_hint.filter(|s| !s.trim().is_empty()),
            }),
            "write" => Some(FinalCondition::Write {
                path_hint: condition.path_hint.filter(|s| !s.trim().is_empty()),
            }),
            "command" => Some(FinalCondition::Command {
                command_hint: condition.command_hint.filter(|s| !s.trim().is_empty()),
            }),
            "none" => None,
            _ => None,
        })
        .collect();

    Some(TurnContract {
        goal: parsed.goal.unwrap_or_default(),
        allowed_to_answer_without_tools: parsed.allowed_to_answer_without_tools.unwrap_or(false),
        final_conditions,
        failure_policy: parsed
            .failure_policy
            .unwrap_or_else(|| "If required evidence is missing, say what is missing.".to_owned()),
    })
}

fn extract_json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    (start <= end).then_some(&content[start..=end])
}

fn latest_user_task(messages: &[ConversationMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| extract_current_task(&message.content))
        .unwrap_or_default()
}

fn recent_contract_context(messages: &[ConversationMessage]) -> String {
    let mut items: Vec<String> = messages
        .iter()
        .rev()
        .filter(|message| message.role == "user" || message.role == "assistant")
        .take(8)
        .map(|message| {
            let content = if message.role == "user" {
                extract_current_task(&message.content)
            } else {
                message.content.trim().to_owned()
            };
            format!(
                "{}: {}",
                message.role,
                truncate_for_contract_context(&content, 700)
            )
        })
        .collect();
    items.reverse();

    if items.is_empty() {
        "(none)".to_owned()
    } else {
        items.join("\n---\n")
    }
}

fn truncate_for_contract_context(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }

    let mut truncated: String = trimmed.chars().take(max_chars).collect();
    truncated.push_str("\n... [truncated]");
    truncated
}

fn extract_current_task(content: &str) -> String {
    let start = "<<<USER_MESSAGE>>>";
    let end = "<<<END_USER_MESSAGE>>>";
    let Some(start_idx) = content.find(start) else {
        return content.trim().to_owned();
    };
    let after_start = start_idx + start.len();
    let Some(end_idx) = content[after_start..].find(end) else {
        return content[after_start..].trim().to_owned();
    };
    content[after_start..after_start + end_idx]
        .trim()
        .to_owned()
}

fn normalize_path_hint(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn normalize_text_hint(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn string_arg<'a>(arguments: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str).map(str::trim)
}

fn write_path_from_execution(result: &ToolExecution) -> Option<String> {
    for key in ["path", "filename"] {
        if let Some(value) = string_arg(&result.arguments, key) {
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }

    serde_json::from_str::<Value>(&result.result)
        .ok()
        .and_then(|value| {
            value
                .get("path")
                .and_then(Value::as_str)
                .or_else(|| value.get("filename").and_then(Value::as_str))
                .map(str::to_owned)
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
        contract_response: Option<String>,
    }

    impl MockModel {
        fn new(responses: Vec<CompletionResult>) -> Self {
            Self {
                responses: AsyncMutex::new(responses.into_iter().collect()),
                requests: Arc::new(AsyncMutex::new(Vec::new())),
                contract_response: None,
            }
        }

        fn with_contract(responses: Vec<CompletionResult>, contract_response: &str) -> Self {
            Self {
                responses: AsyncMutex::new(responses.into_iter().collect()),
                requests: Arc::new(AsyncMutex::new(Vec::new())),
                contract_response: Some(contract_response.to_owned()),
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
            if is_contract_request(messages) {
                return Ok(text_response(self.contract_response.as_deref().unwrap_or(
                    r#"{"goal":"test task","allowed_to_answer_without_tools":true,"final_conditions":[],"failure_policy":"none"}"#,
                )));
            }

            self.requests.lock().await.push(messages.to_vec());
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow!("no mock responses left"))
        }
    }

    fn is_contract_request(messages: &[ChatMessage]) -> bool {
        messages.iter().any(|message| {
            message.role == "system"
                && message
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("Task contract classifier")
        })
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
            "Skipped repeated failed tool call with the same arguments. Re-read the relevant file/output and choose a different, smaller patch or another tool instead of retrying this call."
        );
        assert_eq!(tools.calls.lock().unwrap().len(), 2);
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
    async fn blocks_code_explanation_after_search_without_source_read() {
        let model = MockModel::with_contract(
            vec![
                tool_call_response(
                    "call_1",
                    "search_code_tool",
                    json!({"path": "src/acp.rs", "query": "struct"}),
                ),
                text_response(
                    "`src/acp.rs` is the core ACP server module. It manages sessions and handles JSON-RPC workflow.",
                ),
                tool_call_response(
                    "call_2",
                    "read_file_tool",
                    json!({"path": "src/acp.rs", "start_line": 135, "limit": 80}),
                ),
                text_response(
                    "After reading source, `AcpServer` stores sessions and the model client.",
                ),
            ],
            r#"{"goal":"Explain src/acp.rs","allowed_to_answer_without_tools":false,"final_conditions":[{"type":"source_read","path_hint":"acp.rs","reason":"The user asked about local source file behavior"}],"failure_policy":"Say what source could not be read."}"#,
        );
        let tools = MockTools::with_outputs(HashMap::from([
            (
                "search_code_tool".to_owned(),
                Ok("135:pub struct AcpServer {".to_owned()),
            ),
            (
                "read_file_tool".to_owned(),
                Ok("135:pub struct AcpServer {\n136:    sessions: ...".to_owned()),
            ),
        ]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Explain src/acp.rs.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should recover by reading source");

        assert_eq!(
            result.answer,
            "After reading source, `AcpServer` stores sessions and the model client."
        );
        assert_eq!(result.tool_results.len(), 2);
        assert_eq!(result.tool_results[0].name, "search_code_tool");
        assert_eq!(result.tool_results[1].name, "read_file_tool");

        let requests = model.requests.lock().await;
        let correction_request = requests
            .iter()
            .find(|request| {
                request.iter().any(|message| {
                    message.role == "user"
                        && message
                            .content
                            .as_deref()
                            .unwrap_or_default()
                            .contains("unmet final-answer conditions")
                })
            })
            .expect("guard should add a correction message");
        assert!(!correction_request.iter().any(|message| {
            message
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("core ACP server module")
        }));
    }

    #[tokio::test]
    async fn allows_code_explanation_after_source_read() {
        let model = MockModel::new(vec![
            tool_call_response(
                "call_1",
                "read_file_tool",
                json!({"path": "src/acp.rs", "start_line": 135, "limit": 80}),
            ),
            text_response("`AcpServer` manages sessions based on the source read."),
        ]);
        let tools = MockTools::with_outputs(HashMap::from([(
            "read_file_tool".to_owned(),
            Ok("135:pub struct AcpServer {\n136:    sessions: ...".to_owned()),
        )]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new("user", "Explain src/acp.rs.")],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should accept source-grounded answer");

        assert_eq!(
            result.answer,
            "`AcpServer` manages sessions based on the source read."
        );
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].name, "read_file_tool");
    }

    #[test]
    fn parses_turn_contract_and_extracts_wrapped_task() {
        assert_eq!(
            latest_user_task(&[ConversationMessage::new(
                "user",
                "Task:\n<<<USER_MESSAGE>>>\nexplain acp.rs how it works\n<<<END_USER_MESSAGE>>>\n\nReminder:\n- Use tools",
            )]),
            "explain acp.rs how it works"
        );

        let contract = parse_turn_contract(
            r#"{"goal":"Explain acp.rs","allowed_to_answer_without_tools":false,"final_conditions":[{"type":"source_read","path_hint":"acp.rs","reason":"source file explanation"}],"failure_policy":"Report missing source."}"#,
        )
        .expect("valid contract");

        assert_eq!(contract.goal, "Explain acp.rs");
        assert!(!contract.allowed_to_answer_without_tools);
        assert_eq!(
            contract.final_conditions,
            vec![FinalCondition::SourceRead {
                path_hint: Some("acp.rs".to_owned())
            }]
        );
    }

    #[test]
    fn parses_extended_turn_contract_conditions() {
        let contract = parse_turn_contract(
            r#"{"goal":"Implement and verify","allowed_to_answer_without_tools":false,"final_conditions":[{"type":"web_search","query_hint":"agent context window hallucinations","reason":"user asked for web research"},{"type":"write","path_hint":"src/agent_loop.rs","reason":"implementation requested"},{"type":"command","command_hint":"cargo test","reason":"verification requested"}],"failure_policy":"Continue until evidence exists."}"#,
        )
        .expect("valid contract");

        assert_eq!(
            contract.final_conditions,
            vec![
                FinalCondition::WebSearch {
                    query_hint: Some("agent context window hallucinations".to_owned())
                },
                FinalCondition::Write {
                    path_hint: Some("src/agent_loop.rs".to_owned())
                },
                FinalCondition::Command {
                    command_hint: Some("cargo test".to_owned())
                }
            ]
        );
    }

    #[test]
    fn recent_contract_context_includes_followup_topic() {
        let context = recent_contract_context(&[
            ConversationMessage::new(
                "user",
                "<<<USER_MESSAGE>>>\nhow can i improve this agent context\n<<<END_USER_MESSAGE>>>",
            ),
            ConversationMessage::new(
                "assistant",
                "You should inspect session_store.rs before planning.",
            ),
            ConversationMessage::new("user", "make a plan to implement the RAG system"),
        ]);

        assert!(context.contains("how can i improve this agent context"));
        assert!(context.contains("session_store.rs"));
        assert!(context.contains("make a plan to implement the RAG system"));
    }

    #[test]
    fn evidence_ledger_satisfies_extended_conditions() {
        let tool_results = vec![
            ToolExecution {
                id: "read".to_owned(),
                name: "read_file_tool".to_owned(),
                arguments: json!({"path": "src/agent_loop.rs"})
                    .as_object()
                    .cloned()
                    .unwrap(),
                result: "source".to_owned(),
                error: false,
            },
            ToolExecution {
                id: "web".to_owned(),
                name: "web_search_tool".to_owned(),
                arguments: json!({"query": "agent context window hallucinations"})
                    .as_object()
                    .cloned()
                    .unwrap(),
                result: "results".to_owned(),
                error: false,
            },
            ToolExecution {
                id: "write".to_owned(),
                name: "patch_file_tool".to_owned(),
                arguments: json!({"path": "src/agent_loop.rs"})
                    .as_object()
                    .cloned()
                    .unwrap(),
                result: r#"{"status":"patched"}"#.to_owned(),
                error: false,
            },
            ToolExecution {
                id: "cmd".to_owned(),
                name: "run_command_tool".to_owned(),
                arguments: json!({"cmd": "cargo test"}).as_object().cloned().unwrap(),
                result: "exit_code: 0".to_owned(),
                error: false,
            },
        ];
        let evidence = EvidenceLedger::from_tool_results(&tool_results);

        assert!(evidence.satisfies(&FinalCondition::SourceRead {
            path_hint: Some("agent_loop.rs".to_owned())
        }));
        assert!(evidence.satisfies(&FinalCondition::WebSearch {
            query_hint: Some("context window".to_owned())
        }));
        assert!(evidence.satisfies(&FinalCondition::Write {
            path_hint: Some("src/agent_loop.rs".to_owned())
        }));
        assert!(evidence.satisfies(&FinalCondition::Command {
            command_hint: Some("cargo test".to_owned())
        }));
    }

    #[tokio::test]
    async fn blocks_internet_answer_until_web_search_runs() {
        let model = MockModel::with_contract(
            vec![
                text_response("The best practice is to use RAG."),
                tool_call_response(
                    "call_1",
                    "web_search_tool",
                    json!({"query": "agent context window hallucinations"}),
                ),
                text_response(
                    "After web search, relevant guidance is to retrieve focused context.",
                ),
            ],
            r#"{"goal":"Search web for agent context-window hallucination guidance","allowed_to_answer_without_tools":false,"final_conditions":[{"type":"web_search","query_hint":"context window hallucinations","reason":"The user asked to search the internet"}],"failure_policy":"Ask for a query only if context does not imply one."}"#,
        );
        let tools = MockTools::with_outputs(HashMap::from([(
            "web_search_tool".to_owned(),
            Ok("Search results".to_owned()),
        )]));

        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new(
                "user",
                "search the internet about the one we are discussing",
            )],
            &tools,
            &[],
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should recover with web search");

        assert_eq!(
            result.answer,
            "After web search, relevant guidance is to retrieve focused context."
        );
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].name, "web_search_tool");
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
        assert!(SYSTEM_PROMPT.contains("You are a coding agent"));
        assert!(SYSTEM_PROMPT.contains("Use tools for code-grounded claims"));
        assert!(SYSTEM_PROMPT.contains("Search snippets are not source-read evidence"));
        assert!(SYSTEM_PROMPT.contains("Keep internal reasoning private"));
    }
}
