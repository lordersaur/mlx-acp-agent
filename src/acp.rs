//! ACP protocol handler — JSON-RPC 2.0 over newline-delimited stdio.
//!
//! Mirrors `agent.py`, porting `MlxAcpAgent` to Rust.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tracing::{info, warn};

use crate::agent_loop::{
    AgentLoopOptions, ConversationMessage, ModelClient, SYSTEM_PROMPT, ThoughtHandler,
    ToolExecutor, run_agent_loop,
};
use crate::session_store::{
    CommandSessionInfo, SessionState, build_turn_record, load_persisted_session, new_session,
    persist_session, restore_session, update_command_sessions,
};
use crate::tools::{BuiltinToolRegistry, ToolProgressEvent, ToolProgressSink};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_HISTORY_TURNS: usize = 10;
const MAX_TURN_CHARS: usize = 5000;
const PROTOCOL_VERSION: u64 = 1;

const MODE_PROMPTS: &[(&str, &str)] = &[
    (
        "ask",
        "Current mode: ask.\n\
Prefer inspection and explanation over broad changes.\n\
Read relevant files before answering questions about them.\n\
For abstract questions, inspect the likely defining files or symbols rather than searching the exact phrase.\n\
Identify the object type first and inspect the code that defines that type.\n\
Reuse evidence already gathered instead of re-reading the same files.\n\
Do not make changes unless the user explicitly asks.",
    ),
    (
        "edit",
        "Current mode: edit.\n\
Use tools to inspect files before modifying them.\n\
Make only the changes asked for — do not refactor or clean up surrounding code.\n\
When the request names an abstract concept, inspect the relevant implementation first.\n\
Identify the object type first and inspect the code that defines that type.\n\
Reuse evidence already gathered instead of re-reading the same files.\n\
Validate changes (compile, test) when the user asks or after non-trivial edits.",
    ),
    (
        "fast",
        "/no_think\nCurrent mode: fast.\n\
You may inspect the repo, search the web, fetch URLs, run commands, \
manage terminal sessions, create/edit/delete files, and validate results.\n\
Respond directly and concisely — skip internal reasoning.",
    ),
    (
        "agent",
        "Current mode: agent.\n\
You may inspect the repo, search the web, fetch URLs, run commands, \
manage terminal sessions, create/edit/delete files, and validate results.\n\
Always inspect (read files, list dirs, search code) before answering questions or making changes.\n\
Do not answer repo-specific questions from prior knowledge; read the code first.\n\
For counts, totals, and ranking questions, prefer precise commands or exact file reads over broad search summaries.\n\
For Rust test counts, count `#[test]` and `#[tokio::test]` functions, not `#[cfg(test)]` modules or comments.\n\
For abstract concepts like tools, routes, handlers, entrypoints, config, schema, migrations, models, and env vars, inspect the likely registry, schema, dispatch, or definition code instead of searching the exact phrase.\n\
Identify the object type first: routes, RPC methods, tools, config, tests, commands, files, or schemas.\n\
Do not substitute one object type for another.\n\
For routes or methods, inspect the actual server/router/RPC dispatch code and distinguish them from tools.\n\
If the repo is a protocol server rather than an HTTP app, report the protocol methods or dispatch entries instead of inventing HTTP routes.\n\
For tool totals, verify the complete registry or schema list instead of stopping after one partial file read.\n\
Before another tool call, reuse evidence already gathered and keep a short internal summary of inspected paths, key findings, and the remaining question.",
    ),
];

const TOOL_KINDS: &[(&str, &str)] = &[
    ("read_file_tool", "read"),
    ("list_dir_tool", "read"),
    ("search_code_tool", "search"),
    ("web_search_tool", "search"),
    ("web_fetch_tool", "read"),
    ("run_command_tool", "execute"),
    ("list_command_sessions_tool", "execute"),
    ("start_command_session_tool", "execute"),
    ("read_command_session_tool", "execute"),
    ("write_command_session_tool", "execute"),
    ("terminate_command_session_tool", "execute"),
    ("create_artifact_tool", "edit"),
    ("edit_file_tool", "edit"),
    ("patch_file_tool", "edit"),
    ("delete_path_tool", "edit"),
];

// ---------------------------------------------------------------------------
// Session entry
// ---------------------------------------------------------------------------

struct SessionEntry {
    state: SessionState,
    registry: BuiltinToolRegistry,
}

// ---------------------------------------------------------------------------
// ClientCaller — bidirectional JSON-RPC to Zed
// ---------------------------------------------------------------------------

/// Sends JSON-RPC requests to the Zed client and awaits their responses.
/// Cloneable so it can be shared between `AcpServer` and `ProgressRegistry`.
#[derive(Clone)]
struct ClientCaller {
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: Arc<AtomicU64>,
}

impl ClientCaller {
    fn new(tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(10_000)), // start high to avoid clashing with Zed's IDs
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, reply_tx);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Ok(s) = serde_json::to_string(&request) {
            self.tx.send(s).ok();
        }
        let response = reply_rx.await?;
        if let Some(err) = response.get("error") {
            anyhow::bail!("client error from {method}: {err}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Route an incoming response message to the waiting caller.
    fn route_response(&self, msg: &Value) -> bool {
        let id = match msg.get("id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return false,
        };
        if msg.get("result").is_none() && msg.get("error").is_none() {
            return false;
        }
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            tx.send(msg.clone()).ok();
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// ACP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AcpServer {
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    model: Arc<dyn ModelClient>,
    active_tasks: Arc<Mutex<HashMap<String, (u64, AbortHandle)>>>,
    next_task_id: Arc<AtomicU64>,
    tx: mpsc::UnboundedSender<String>,
    caller: ClientCaller,
    has_terminal: Arc<std::sync::atomic::AtomicBool>,
}

fn spawn_loop(
    model: Arc<dyn ModelClient>,
    tx: mpsc::UnboundedSender<String>,
    session_id: String,
    messages: Vec<ConversationMessage>,
    progress_registry: ProgressRegistry,
    tool_schemas: Vec<Value>,
    options: AgentLoopOptions,
    active_reasoning: Arc<Mutex<Option<String>>>,
) -> (
    tokio::task::JoinHandle<Result<crate::agent_loop::LoopResult>>,
    tokio::task::AbortHandle,
) {
    let handle = tokio::task::spawn(async move {
        let mut thought_handler = AcpThoughtHandler {
            session_id,
            tx,
            active_reasoning: Some(active_reasoning),
        };
        run_agent_loop(
            &*model,
            &messages,
            &progress_registry,
            &tool_schemas,
            Some(&mut thought_handler),
            options,
        )
        .await
    });
    let abort_handle = handle.abort_handle();
    (handle, abort_handle)
}

impl AcpServer {
    fn new(model: Arc<dyn ModelClient>, tx: mpsc::UnboundedSender<String>) -> Self {
        let caller = ClientCaller::new(tx.clone());
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            model,
            active_tasks: Arc::new(Mutex::new(HashMap::new())),
            next_task_id: Arc::new(AtomicU64::new(1)),
            tx,
            caller,
            has_terminal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    // -----------------------------------------------------------------------
    // Low-level send helpers
    // -----------------------------------------------------------------------

    fn send_raw(&self, msg: Value) {
        match serde_json::to_string(&msg) {
            Ok(s) => {
                self.tx.send(s).ok();
            }
            Err(e) => warn!("failed to serialize message: {e}"),
        }
    }

    fn send_response(&self, id: Value, result: Value) {
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }));
    }

    fn send_error(&self, id: Value, code: i64, message: &str) {
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }));
    }

    fn send_notification(&self, method: &str, params: Value) {
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    fn send_session_update(&self, session_id: &str, update: Value) {
        self.send_notification(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": update,
            }),
        );
    }

    fn send_thought(&self, session_id: &str, text: &str) {
        let normalized = text.trim();
        if normalized.is_empty() {
            return;
        }
        self.send_session_update(
            session_id,
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": format!("{normalized}\n\n")},
            }),
        );
    }

    fn send_message(&self, session_id: &str, text: &str) {
        self.send_session_update(
            session_id,
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            }),
        );
    }

    // -----------------------------------------------------------------------
    // Main message dispatch
    // -----------------------------------------------------------------------

    async fn handle_message(self: Arc<Self>, msg: Value) {
        // If this is a response to one of our outgoing requests, route it.
        if self.caller.route_response(&msg) {
            return;
        }
        let method = match msg.get("method").and_then(|v| v.as_str()) {
            Some(m) => m.to_owned(),
            None => return,
        };

        let has_id = msg.get("id").is_some();
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let params = msg
            .get("params")
            .cloned()
            .unwrap_or(Value::Object(Map::new()));

        if has_id {
            match self.dispatch_request(&method, params).await {
                Ok(result) => self.send_response(id, result),
                Err(e) => {
                    warn!("request {method} error: {e}");
                    self.send_error(id, -32603, &e.to_string());
                }
            }
        } else {
            self.dispatch_notification(&method, params).await;
        }
    }

    async fn dispatch_request(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "initialize" => self.handle_initialize(params).await,
            "authenticate" => self.handle_authenticate(params).await,
            "session/new" => self.handle_session_new(params).await,
            "session/load" => self.handle_session_load(params).await,
            "session/prompt" => self.handle_session_prompt(params).await,
            "session/set_mode" => self.handle_session_set_mode(params).await,
            other => {
                // Extension methods (prefixed with _)
                if let Some(name) = other.strip_prefix('_') {
                    Ok(json!({"ok": true, "method": name}))
                } else {
                    anyhow::bail!("method not found: {other}")
                }
            }
        }
    }

    async fn dispatch_notification(&self, method: &str, params: Value) {
        match method {
            "session/cancel" => self.handle_session_cancel(params).await,
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Request handlers
    // -----------------------------------------------------------------------

    async fn handle_initialize(&self, params: Value) -> Result<Value> {
        let client_info = params.get("clientInfo");
        let has_terminal = params
            .get("clientCapabilities")
            .and_then(|c| c.get("terminal"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.has_terminal.store(has_terminal, Ordering::Relaxed);
        info!(
            "initialize from {:?} has_terminal={has_terminal}",
            client_info
        );
        Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": {},
            "agentInfo": {
                "name": "mlx-acp-agent",
                "title": "Local MLX ACP Agent",
                "version": "0.3.0",
            },
        }))
    }

    async fn handle_authenticate(&self, params: Value) -> Result<Value> {
        let method_id = params
            .get("methodId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        info!("authenticate: {method_id}");
        Ok(json!({}))
    }

    async fn handle_session_new(&self, params: Value) -> Result<Value> {
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_owned();
        let mut state = new_session(&cwd);
        hydrate_command_sessions_for_cwd(&mut state);
        let session_id = state.session_id.clone();
        let mode_id = state.mode_id.clone();

        // Persist the fresh session so the ID is durable from the start.
        persist_session(&state);

        let registry = build_registry(&cwd, self.model.clone());
        {
            let mut map = self.sessions.lock().unwrap();
            map.insert(session_id.clone(), SessionEntry { state, registry });
        }

        info!("session/new cwd={cwd} session_id={session_id}");
        Ok(json!({
            "sessionId": session_id,
            "modes": {
                "availableModes": session_modes(),
                "currentModeId": mode_id,
            },
        }))
    }

    async fn handle_session_load(&self, params: Value) -> Result<Value> {
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_owned();
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        {
            let mut map = self.sessions.lock().unwrap();
            if let Some(entry) = map.get_mut(&session_id) {
                // Session already in memory — just update cwd.
                entry.state.cwd = cwd.clone();
                hydrate_command_sessions_for_cwd(&mut entry.state);
            } else {
                // Try to restore from disk first, otherwise create fresh.
                let mut state = if let Some(persisted) = load_persisted_session(&session_id) {
                    info!(
                        "session/load: restored {} turns from disk for {session_id}",
                        persisted.turns.len()
                    );
                    restore_session(persisted, &cwd)
                } else {
                    crate::session_store::SessionState {
                        session_id: session_id.clone(),
                        cwd: cwd.clone(),
                        mode_id: "agent".to_owned(),
                        turns: Vec::new(),
                        active_command_sessions: HashMap::new(),
                        interrupted: false,
                        pending_task: None,
                        active_tool_calls: HashMap::new(),
                        tool_event_counter: 0,
                    }
                };
                hydrate_command_sessions_for_cwd(&mut state);
                let registry = build_registry(&cwd, self.model.clone());
                map.insert(session_id.clone(), SessionEntry { state, registry });
            }
        }

        info!("session/load cwd={cwd} session_id={session_id}");
        Ok(json!({}))
    }

    async fn handle_session_set_mode(&self, params: Value) -> Result<Value> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let mode_id = params
            .get("modeId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        info!("session/set_mode session_id={session_id} mode={mode_id}");
        let valid_modes = ["ask", "edit", "agent", "fast"];
        if valid_modes.contains(&mode_id.as_str()) {
            let mut map = self.sessions.lock().unwrap();
            if let Some(entry) = map.get_mut(&session_id) {
                entry.state.mode_id = mode_id;
            }
        }
        Ok(json!({}))
    }

    async fn handle_session_prompt(&self, params: Value) -> Result<Value> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        info!("session/prompt session_id={session_id}");

        // Ensure session exists
        {
            let mut map = self.sessions.lock().unwrap();
            if !map.contains_key(&session_id) {
                let mut state = crate::session_store::SessionState {
                    session_id: session_id.clone(),
                    cwd: ".".to_owned(),
                    mode_id: "agent".to_owned(),
                    turns: Vec::new(),
                    active_command_sessions: HashMap::new(),
                    interrupted: false,
                    pending_task: None,
                    active_tool_calls: HashMap::new(),
                    tool_event_counter: 0,
                };
                hydrate_command_sessions_for_cwd(&mut state);
                let registry = build_registry(".", self.model.clone());
                map.insert(session_id.clone(), SessionEntry { state, registry });
            }
        }

        // Extract user text from prompt blocks
        let user_text = extract_prompt_text(&params);
        if user_text.trim().is_empty() {
            self.send_message(&session_id, "Empty prompt.");
            return Ok(json!({"stopReason": "end_turn"}));
        }

        // Snapshot session state and clone registry (cheap Arc clone)
        let (
            mode_id,
            turns,
            _cwd,
            interrupted,
            pending_task,
            active_cmd_sessions,
            pending_terminal_outputs,
            registry,
        ) = {
            let mut map = self.sessions.lock().unwrap();
            let entry = map.get_mut(&session_id).unwrap();
            let pending_terminal_outputs =
                poll_active_command_sessions(&entry.registry, &mut entry.state);
            (
                entry.state.mode_id.clone(),
                entry.state.turns.clone(),
                entry.state.cwd.clone(),
                entry.state.interrupted,
                entry.state.pending_task.clone(),
                entry.state.active_command_sessions.clone(),
                pending_terminal_outputs,
                entry.registry.clone(),
            )
        };
        let resume_request =
            interrupted && is_continue_prompt(&user_text) && pending_task.is_some();
        let effective_user_text = if resume_request {
            format!(
                "Continue the previously interrupted task: {}",
                pending_task.clone().unwrap_or_default()
            )
        } else {
            user_text.clone()
        };

        // Build conversation messages
        let messages = build_messages(
            &mode_id,
            &turns,
            interrupted,
            pending_task.as_deref(),
            &active_cmd_sessions,
            &pending_terminal_outputs,
            &user_text,
        );

        let tool_schemas = registry.tool_schemas();

        // Share model reasoning between the thought handler and tool executor
        let active_reasoning = Arc::new(Mutex::new(None));

        // Wrap registry with progress reporting
        let progress_sink = Arc::new(AcpProgressSink {
            tx: self.tx.clone(),
            session_id: session_id.clone(),
        });
        let progress_registry = ProgressRegistry {
            inner: registry.with_progress_sink(progress_sink),
            tx: self.tx.clone(),
            session_id: session_id.clone(),
            counter: Arc::new(AtomicU64::new(0)),
            caller: self.caller.clone(),
            has_terminal: self.has_terminal.load(Ordering::Relaxed),
            active_reasoning: active_reasoning.clone(),
            terminal_sessions: Arc::new(Mutex::new(HashMap::new())),
        };

        let options = AgentLoopOptions::default();

        // Send initial thought
        // self.send_thought(&session_id, &format!("Mode: {mode_id}. Working..."));

        let my_task_id = self.next_task_id.fetch_add(1, Ordering::SeqCst);

        // Spawn the agent loop as a cancellable task. We do this under a lock
        // to ensure we don't have multiple active loops for the same session.
        let (handle, is_newest) = {
            let mut tasks = self.active_tasks.lock().unwrap();

            // Check if we were already replaced by a newer prompt before we even got the lock.
            if let Some((tid, _)) = tasks.get(&session_id) {
                let tid = *tid;
                if tid > my_task_id {
                    // We are already obsolete.
                    (None, false)
                } else {
                    // We are replacing an older task.
                    let (_, old_handle) = tasks.remove(&session_id).unwrap();
                    info!(
                        "aborting previous task {tid} for session {session_id} (we are {my_task_id})"
                    );
                    old_handle.abort();

                    let (h, ah) = spawn_loop(
                        self.model.clone(),
                        self.tx.clone(),
                        session_id.clone(),
                        messages,
                        progress_registry,
                        tool_schemas,
                        options,
                        active_reasoning,
                    );
                    tasks.insert(session_id.clone(), (my_task_id, ah));
                    (Some(h), true)
                }
            } else {
                // No task active, we are the first or the old one already finished.
                let (h, ah) = spawn_loop(
                    self.model.clone(),
                    self.tx.clone(),
                    session_id.clone(),
                    messages,
                    progress_registry,
                    tool_schemas,
                    options,
                    active_reasoning,
                );
                tasks.insert(session_id.clone(), (my_task_id, ah));
                (Some(h), true)
            }
        };

        if !is_newest {
            info!("task {my_task_id} for session {session_id} was born obsolete; skipping");
            return Ok(json!({"stopReason": "end_turn"}));
        }
        let handle = handle.unwrap();

        // Wait for the loop to finish or be cancelled
        let loop_result = match handle.await {
            Ok(Ok(result)) => Some(result),
            Ok(Err(e)) => {
                warn!("agent loop error: {e}");
                None
            }
            Err(e) if e.is_cancelled() => {
                // Cancelled via session/cancel or new prompt
                None
            }
            Err(e) => {
                warn!("agent loop panicked: {e}");
                None
            }
        };

        // Only cleanup if we are still the current task for this session.
        // This prevents HP1 from removing HP2's handle.
        let is_still_current = {
            let mut tasks = self.active_tasks.lock().unwrap();
            if let Some((tid, _)) = tasks.get(&session_id) {
                if *tid == my_task_id {
                    tasks.remove(&session_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        // If we were replaced by a newer prompt task, stop immediately.
        // This prevents overlapping UI updates and mixed state.
        if !is_still_current {
            return Ok(json!({"stopReason": "end_turn"}));
        }

        if let Some(result) = loop_result {
            // Update session state
            {
                let mut map = self.sessions.lock().unwrap();
                if let Some(entry) = map.get_mut(&session_id) {
                    entry
                        .state
                        .turns
                        .push(build_turn_record("user", &effective_user_text, &[]));
                    entry.state.turns.push(build_turn_record(
                        "assistant",
                        &result.answer,
                        &result.tool_results,
                    ));
                    // Trim in-memory history
                    if entry.state.turns.len() > MAX_HISTORY_TURNS * 2 {
                        let drain_to = entry.state.turns.len() - MAX_HISTORY_TURNS * 2;
                        entry.state.turns.drain(..drain_to);
                    }
                    update_command_sessions(&mut entry.state, &result.tool_results);
                    entry.state.interrupted = false;
                    entry.state.pending_task = None;

                    // Persist durable history to disk.
                    persist_session(&entry.state);
                }
            }
            self.send_message(&session_id, &result.answer);
        } else {
            // Interrupted — persist so "continue" works after restart.
            {
                let mut map = self.sessions.lock().unwrap();
                if let Some(entry) = map.get_mut(&session_id) {
                    entry.state.interrupted = true;
                    entry.state.pending_task = if resume_request {
                        pending_task
                    } else {
                        Some(user_text)
                    };
                    persist_session(&entry.state);
                }
            }
            // self.send_message(&session_id, "Interrupted. Say `continue` to resume.");
        }

        Ok(json!({"stopReason": "end_turn"}))
    }

    // -----------------------------------------------------------------------
    // Notification handlers
    // -----------------------------------------------------------------------

    async fn handle_session_cancel(&self, params: Value) {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        info!("session/cancel session_id={session_id}");

        // Mark interrupted
        {
            let mut map = self.sessions.lock().unwrap();
            if let Some(entry) = map.get_mut(&session_id) {
                entry.state.interrupted = true;
            }
        }
        // Abort the active task
        if let Some((_tid, handle)) = self.active_tasks.lock().unwrap().remove(&session_id) {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// ACP server entry point
// ---------------------------------------------------------------------------

pub async fn run(model: Arc<dyn ModelClient>) -> Result<()> {
    // Prune history files older than 7 days on startup.
    crate::session_store::prune_old_history(std::time::Duration::from_secs(7 * 24 * 3600));

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Writer task: serialize all outgoing JSON-RPC messages to stdout
    let writer_task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.write_all(b"\n").await.is_err() {
                break;
            }
            stdout.flush().await.ok();
        }
    });

    let server = Arc::new(AcpServer::new(model, tx));

    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                warn!("failed to parse JSON-RPC message: {e}");
                continue;
            }
        };
        let server_clone = server.clone();
        tokio::spawn(async move {
            server_clone.handle_message(msg).await;
        });
    }

    writer_task.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// ThoughtHandler
// ---------------------------------------------------------------------------

struct AcpThoughtHandler {
    session_id: String,
    tx: mpsc::UnboundedSender<String>,
    active_reasoning: Option<Arc<Mutex<Option<String>>>>,
}

impl AcpThoughtHandler {
    /// Send a complete thought with trailing separator.
    fn send_thought(&self, text: &str) {
        let normalized = text.trim();
        if normalized.is_empty() {
            return;
        }
        if let Some(ref slot) = self.active_reasoning {
            let mut lock = slot.lock().unwrap();
            *lock = Some(normalized.to_owned());
        }
        self.send_raw_chunk(&format!("{normalized}\n\n"));
    }

    /// Send a raw text chunk with no decoration (used for streaming tokens).
    fn send_raw_chunk(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": text},
                },
            },
        });
        if let Ok(s) = serde_json::to_string(&msg) {
            self.tx.send(s).ok();
        }
    }
}

#[async_trait]
impl ThoughtHandler for AcpThoughtHandler {
    async fn on_thought(&mut self, thought: &str) {
        self.send_thought(thought);
    }

    async fn on_thought_chunk(&mut self, chunk: &str) {
        // Accumulate into active_reasoning for tool-call attribution.
        if let Some(ref slot) = self.active_reasoning {
            let mut lock = slot.lock().unwrap();
            let entry = lock.get_or_insert_with(String::new);
            entry.push_str(chunk);
        }
        self.send_raw_chunk(chunk);
    }

    async fn on_thought_end(&mut self) {
        // Flush a separator so Zed visually terminates the streaming thought block.
        self.send_raw_chunk("\n\n");
    }
}

// ---------------------------------------------------------------------------
// Tool progress sink
// ---------------------------------------------------------------------------

struct AcpProgressSink {
    tx: mpsc::UnboundedSender<String>,
    session_id: String,
}

impl AcpProgressSink {
    fn send_thought(&self, text: &str) {
        let normalized = text.trim();
        if normalized.is_empty() {
            return;
        }
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": format!("{normalized}\n\n")},
                },
            },
        });
        if let Ok(s) = serde_json::to_string(&msg) {
            self.tx.send(s).ok();
        }
    }
}

impl ToolProgressSink for AcpProgressSink {
    fn emit(&self, event: ToolProgressEvent) {
        match event {
            ToolProgressEvent::Reasoning { summary } => self.send_thought(&summary),
            ToolProgressEvent::FileModified { path, status } => {
                let mut chars = status.chars();
                let capitalized = match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => "Updated".to_owned(),
                };
                self.send_thought(&format!("{capitalized} `{path}`."));
            }
            ToolProgressEvent::TerminalOutput { session_id, output } => {
                self.send_thought(&format!("Terminal `{session_id}` output:\n{output}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ProgressRegistry — wraps BuiltinToolRegistry with ACP tool call events
// ---------------------------------------------------------------------------

struct ProgressRegistry {
    inner: BuiltinToolRegistry,
    tx: mpsc::UnboundedSender<String>,
    session_id: String,
    counter: Arc<AtomicU64>,
    caller: ClientCaller,
    has_terminal: bool,
    active_reasoning: Arc<Mutex<Option<String>>>,
    /// Maps synthetic session IDs (termsess_*) → ACP terminal IDs for terminal-backed sessions.
    terminal_sessions: Arc<Mutex<HashMap<String, String>>>,
}

// SAFETY: BuiltinToolRegistry is Send+Sync (internal state behind Arc<Mutex>),
// mpsc::UnboundedSender<T> is Send, AtomicU64 is Send+Sync.
unsafe impl Send for ProgressRegistry {}
unsafe impl Sync for ProgressRegistry {}

impl ProgressRegistry {
    fn send(&self, msg: Value) {
        if let Ok(s) = serde_json::to_string(&msg) {
            self.tx.send(s).ok();
        }
    }

    fn send_thought_raw(&self, text: &str) {
        let normalized = text.trim();
        if normalized.is_empty() {
            return;
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": format!("{normalized}\n\n")},
                },
            },
        }));
    }

    fn next_tool_call_id(&self, name: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{name}:{n}")
    }

    /// Start a command via `terminal/create` (no PTY double-run).
    /// Returns immediately with a synthetic session_id the agent uses to poll.
    /// A background task waits for exit and sends the Zed completion update.
    async fn invoke_start_session_with_terminal(
        &self,
        tool_call_id: String,
        arguments: &Map<String, Value>,
    ) -> Result<String> {
        let cmd = arguments
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let cwd = self.inner.workspace_cwd().to_string_lossy().into_owned();
        let (terminal_command, terminal_args) = terminal_command_and_args(&cmd);

        let create_result = self
            .caller
            .call(
                "terminal/create",
                json!({
                    "sessionId": self.session_id,
                    "command": terminal_command,
                    "args": terminal_args,
                    "cwd": cwd,
                    "outputByteLimit": 12000,
                }),
            )
            .await;

        let terminal_id = match create_result {
            Ok(v) => match v
                .get("terminalId")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            {
                Some(id) => id,
                None => {
                    return self
                        .inner
                        .invoke("start_command_session_tool", arguments.clone())
                        .await;
                }
            },
            Err(_) => {
                return self
                    .inner
                    .invoke("start_command_session_tool", arguments.clone())
                    .await;
            }
        };

        let synthetic_sid = format!("termsess_{terminal_id}");
        self.terminal_sessions
            .lock()
            .unwrap()
            .insert(synthetic_sid.clone(), terminal_id.clone());

        // Show live terminal widget in the tool call panel.
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "status": "in_progress",
                    "content": [{"type": "terminal", "terminalId": terminal_id}],
                },
            },
        }));

        // Background task: wait for exit, send completion update, release terminal.
        let caller = self.caller.clone();
        let acp_session_id = self.session_id.clone();
        let tx = self.tx.clone();
        let terminal_id_bg = terminal_id.clone();
        let tool_call_id_bg = tool_call_id.clone();
        let cmd_bg = cmd.clone();

        tokio::spawn(async move {
            let exit_result: Result<Value> = match tokio::time::timeout(
                std::time::Duration::from_secs(300),
                caller.call(
                    "terminal/wait_for_exit",
                    json!({
                        "sessionId": acp_session_id,
                        "terminalId": terminal_id_bg,
                    }),
                ),
            )
            .await
            {
                Ok(inner) => inner,
                Err(_) => Err(anyhow::anyhow!("timeout")),
            };

            let exit_code = exit_result
                .ok()
                .as_ref()
                .and_then(extract_terminal_exit_code)
                .unwrap_or(0);
            let status = if exit_code == 0 { "completed" } else { "error" };
            let summary = format!("`{cmd_bg}` exited with code {exit_code}.");

            let msg = json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": acp_session_id,
                    "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": tool_call_id_bg,
                        "status": status,
                        "content": [{"type": "content", "content": {"type": "text", "text": summary}}],
                    },
                },
            });
            if let Ok(s) = serde_json::to_string(&msg) {
                tx.send(s).ok();
            }

            // Do NOT remove session mapping here — the agent may not have polled yet.
            // invoke_read_terminal_session will clean up when it detects completion.
        });

        Ok(format!(
            "session_id: {synthetic_sid}\n\nrunning: true\n\nstdout:\n[live output visible in the terminal panel]\n"
        ))
    }

    /// Read output from a terminal-backed session by calling `terminal/output`.
    async fn invoke_read_terminal_session(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<String> {
        let v = self
            .caller
            .call(
                "terminal/output",
                json!({
                    "sessionId": self.session_id,
                    "terminalId": terminal_id,
                }),
            )
            .await?;

        let output = extract_terminal_output(&v);
        // exit_status is nested when returned from terminal/output
        let exit_code = v
            .get("exitStatus")
            .and_then(|s| s.get("exitCode"))
            .and_then(|c| c.as_i64())
            .or_else(|| v.get("exitCode").and_then(|c| c.as_i64()));

        if let Some(exit_code) = exit_code {
            // Terminal has completed — clean up mapping and release.
            self.terminal_sessions.lock().unwrap().remove(session_id);
            self.caller
                .call(
                    "terminal/release",
                    json!({
                        "sessionId": self.session_id,
                        "terminalId": terminal_id,
                    }),
                )
                .await
                .ok();
            Ok(format!(
                "session_id: {session_id}\n\nexit_code: {exit_code}\n\nrunning: false\n\nstdout:\n{output}\n\nstderr:\n"
            ))
        } else {
            Ok(format!(
                "session_id: {session_id}\n\nrunning: true\n\nstdout:\n{output}\n\n[command still running — call read_command_session_tool again to check progress]"
            ))
        }
    }

    /// Kill and release a terminal-backed session.
    async fn invoke_terminate_terminal_session(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<String> {
        self.caller
            .call(
                "terminal/kill",
                json!({
                    "sessionId": self.session_id,
                    "terminalId": terminal_id,
                }),
            )
            .await
            .ok();
        self.caller
            .call(
                "terminal/release",
                json!({
                    "sessionId": self.session_id,
                    "terminalId": terminal_id,
                }),
            )
            .await
            .ok();
        self.terminal_sessions.lock().unwrap().remove(session_id);
        Ok(format!("Terminal session `{session_id}` terminated."))
    }

    async fn invoke_via_terminal(
        &self,
        tool_call_id: String,
        arguments: &Map<String, Value>,
    ) -> Result<String> {
        let cmd = arguments
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let cwd = self.inner.workspace_cwd().to_string_lossy().into_owned();
        let (terminal_command, terminal_args) = terminal_command_and_args(&cmd);

        // Create the ACP terminal — Zed will display it live in the panel.
        let create_result = self
            .caller
            .call(
                "terminal/create",
                json!({
                    "sessionId": self.session_id,
                    "command": terminal_command,
                    "args": terminal_args,
                    "cwd": cwd,
                    "outputByteLimit": 12000,
                }),
            )
            .await;

        let terminal_id = match create_result {
            Ok(v) => match v
                .get("terminalId")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            {
                Some(id) => id,
                None => {
                    // Client doesn't support terminal despite capability — fall back.
                    return self
                        .inner
                        .invoke("run_command_tool", arguments.clone())
                        .await;
                }
            },
            Err(_) => {
                return self
                    .inner
                    .invoke("run_command_tool", arguments.clone())
                    .await;
            }
        };

        // Attach the terminal to the tool call panel so Zed shows live output.
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "status": "in_progress",
                    "content": [{"type": "terminal", "terminalId": terminal_id}],
                },
            },
        }));

        // Wait for the command to finish. The ACP schema has no timeout field — Zed waits
        // until the process exits. We add a client-side 5-minute timeout so the agent
        // doesn't hang forever if the command never terminates.
        let exit_result: Result<Value> = match tokio::time::timeout(
            std::time::Duration::from_secs(300),
            self.caller.call(
                "terminal/wait_for_exit",
                json!({
                    "sessionId": self.session_id,
                    "terminalId": terminal_id,
                }),
            ),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => Err(anyhow::anyhow!(
                "terminal/wait_for_exit timed out after 300s"
            )),
        };

        let output_result = self
            .caller
            .call(
                "terminal/output",
                json!({
                    "sessionId": self.session_id,
                    "terminalId": terminal_id,
                }),
            )
            .await;

        let exit_payload = exit_result.ok();
        let output_payload = output_result.ok();

        let exit_code = exit_payload
            .as_ref()
            .and_then(extract_terminal_exit_code)
            .or_else(|| {
                output_payload.as_ref().and_then(|v| {
                    v.get("exitStatus")
                        .and_then(|s| s.get("exitCode"))
                        .and_then(|c| c.as_i64())
                        .or_else(|| v.get("exitCode").and_then(|c| c.as_i64()))
                })
            });
        let output = output_payload
            .as_ref()
            .map(extract_terminal_output)
            .unwrap_or_default();

        // If ACP terminal interaction failed entirely, fall back to the normal
        // command implementation instead of fabricating a failure.
        if exit_code.is_none() && output.is_empty() {
            self.caller
                .call(
                    "terminal/release",
                    json!({
                        "sessionId": self.session_id,
                        "terminalId": terminal_id,
                    }),
                )
                .await
                .ok();
            return self
                .inner
                .invoke("run_command_tool", arguments.clone())
                .await;
        }

        let exit_code = exit_code.unwrap_or(0);
        let formatted_output = format!(
            "$ {cmd}

exit_code: {exit_code}

stdout:
{output}

stderr:
"
        );
        let update_text = render_tool_finished("run_command_tool", arguments, &formatted_output)
            .unwrap_or_else(|| format!("`{cmd}` exited with code {exit_code}."));
        let status = if exit_code == 0 { "completed" } else { "error" };

        // Mark the tool call as finished with explicit text + raw output. Sending
        // a terminal-only completion update can leave the panel visually stuck in
        // progress even though the command and model turn already completed.
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "status": status,
                    "content": [{"type": "content", "content": {"type": "text", "text": update_text}}],
                    "rawOutput": parse_tool_output(&formatted_output),
                },
            },
        }));

        // Release the terminal after the completion update has been sent.
        self.caller
            .call(
                "terminal/release",
                json!({
                    "sessionId": self.session_id,
                    "terminalId": terminal_id,
                }),
            )
            .await
            .ok();

        Ok(formatted_output)
    }
}

#[async_trait]
impl ToolExecutor for ProgressRegistry {
    fn tool_names(&self) -> Vec<String> {
        self.inner.tool_names()
    }

    async fn invoke(&self, name: &str, arguments: Map<String, Value>) -> Result<String> {
        let tool_call_id = self.next_tool_call_id(name);

        // ACP tool_call event
        let kind = tool_kind(name);
        let title = render_tool_title(name, &arguments);

        // Keep model reasoning in the thought stream, not inside the expandable
        // tool-call panel. Tool panels should describe the tool action itself.
        self.active_reasoning.lock().unwrap().take();
        let initial_text =
            render_tool_started(name, &arguments).unwrap_or_else(|| "Starting.".to_owned());

        let mut start_update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": title,
            "kind": kind,
            "status": "in_progress",
            "content": [{"type": "content", "content": {"type": "text", "text": initial_text}}],
        });
        if let Some(p) = arguments.get("path").and_then(|v| v.as_str()) {
            start_update["locations"] = json!([{"path": p}]);
        }
        let raw_input: Map<String, Value> = arguments
            .iter()
            .filter(|(_, v)| !v.is_null() && v.as_str() != Some(""))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !raw_input.is_empty() {
            start_update["rawInput"] = Value::Object(raw_input);
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": start_update,
            },
        }));

        if name == "run_command_tool" && self.has_terminal {
            return self.invoke_via_terminal(tool_call_id, &arguments).await;
        }
        if name == "start_command_session_tool" && self.has_terminal {
            return self
                .invoke_start_session_with_terminal(tool_call_id, &arguments)
                .await;
        }

        // Route read/terminate through terminal/output or terminal/kill when the
        // session_id refers to a terminal-backed session (termsess_* prefix).
        let terminal_id_for_session =
            if name == "read_command_session_tool" || name == "terminate_command_session_tool" {
                arguments
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .and_then(|sid| {
                        self.terminal_sessions
                            .lock()
                            .unwrap()
                            .get(sid)
                            .cloned()
                            .map(|tid| (sid.to_owned(), tid))
                    })
            } else {
                None
            };

        let result = if let Some((sid, tid)) = terminal_id_for_session {
            if name == "read_command_session_tool" {
                self.invoke_read_terminal_session(&sid, &tid).await
            } else {
                self.invoke_terminate_terminal_session(&sid, &tid).await
            }
        } else {
            self.inner.invoke(name, arguments.clone()).await
        };

        // ACP tool_call_update event
        let (update_text, raw_output, is_error) = match &result {
            Ok(output) => {
                let update_text = render_tool_finished(name, &arguments, output)
                    .unwrap_or_else(|| "Completed.".to_owned());
                (update_text, Some(parse_tool_output(output)), false)
            }
            Err(e) => (
                render_tool_error(name, &arguments, &format!("{e:?}")),
                Some(Value::String(format!("{e:?}"))),
                true,
            ),
        };

        let status = if is_error { "error" } else { "completed" };
        let mut update = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "status": status,
                    "content": [{"type": "content", "content": {"type": "text", "text": update_text}}],
                },
            },
        });
        if let Some(raw_output) = raw_output {
            update["params"]["update"]["rawOutput"] = raw_output;
        }
        self.send(update);

        result
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_registry(cwd: &str, model: Arc<dyn ModelClient>) -> BuiltinToolRegistry {
    match BuiltinToolRegistry::new_with_model(cwd, model.clone()) {
        Ok(r) => r,
        Err(_) => BuiltinToolRegistry::new_with_model(".", {
            // Use the same model
            model.clone()
        })
        .unwrap_or_else(|_| {
            // Last resort: create without model
            BuiltinToolRegistry::new(".").expect("current dir should exist")
        }),
    }
}

fn hydrate_command_sessions_for_cwd(state: &mut SessionState) {
    let cwd = std::path::Path::new(&state.cwd);
    let Ok(snapshots) = crate::tools::command::load_persisted_command_sessions_for_cwd(cwd) else {
        return;
    };

    state.active_command_sessions = snapshots
        .into_iter()
        .filter(|session| session.running)
        .map(|session| {
            (
                session.session_id.clone(),
                CommandSessionInfo {
                    session_id: session.session_id,
                    cmd: session.cmd,
                    last_output: session.last_output,
                    running: session.running,
                    exit_code: session.exit_code,
                },
            )
        })
        .collect();
}

fn poll_active_command_sessions(
    registry: &BuiltinToolRegistry,
    state: &mut SessionState,
) -> Vec<String> {
    let session_ids: Vec<String> = state
        .active_command_sessions
        .values()
        .filter(|session| session.running)
        .map(|session| session.session_id.clone())
        .collect();

    let mut tool_results = Vec::new();
    let mut outputs = Vec::new();

    for session_id in session_ids {
        let Ok(result) =
            crate::tools::command::read_command_session(&registry.cmd_sessions, &session_id, 1200)
        else {
            continue;
        };

        if let Some(output) = result.get("output").and_then(|v| v.as_str()) {
            let trimmed = output.trim();
            if !trimmed.is_empty() {
                outputs.push(format!("Terminal `{session_id}` output:\n{trimmed}"));
            }
        }

        tool_results.push(crate::agent_loop::ToolExecution {
            id: String::new(),
            name: "read_command_session_tool".to_owned(),
            arguments: Map::from_iter([("session_id".to_owned(), Value::String(session_id))]),
            result: result.to_string(),
            error: false,
        });
    }

    update_command_sessions(state, &tool_results);
    outputs
}

fn session_modes() -> Value {
    json!([
        {
            "id": "ask",
            "name": "Ask",
            "description": "Answer questions and inspect code conservatively.",
        },
        {
            "id": "edit",
            "name": "Edit",
            "description": "Focus on file changes: read, patch, edit, create.",
        },
        {
            "id": "agent",
            "name": "Agent",
            "description": "Full coding-agent mode with repo search, web access, shell, edits, and validation.",
        },
        {
            "id": "fast",
            "name": "Fast",
            "description": "Agent mode without extended thinking — faster responses.",
        },
    ])
}

fn extract_prompt_text(params: &Value) -> String {
    let Some(prompt) = params.get("prompt").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let parts: Vec<String> = prompt
        .iter()
        .filter_map(|block| {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                Some(text.to_owned())
            } else if let Some(uri) = block.get("uri").and_then(|v| v.as_str()) {
                Some(format!("[resource] {uri}"))
            } else {
                None
            }
        })
        .collect();
    parts.join("\n").trim().to_owned()
}

fn build_messages(
    mode_id: &str,
    turns: &[crate::session_store::TurnRecord],
    interrupted: bool,
    pending_task: Option<&str>,
    active_cmd_sessions: &HashMap<String, CommandSessionInfo>,
    pending_terminal_outputs: &[String],
    user_text: &str,
) -> Vec<ConversationMessage> {
    let mut messages = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    let resume_request = interrupted && is_continue_prompt(user_text) && pending_task.is_some();
    let effective_user_text = if resume_request {
        format!(
            "Continue the previously interrupted task: {}",
            pending_task.unwrap_or_default()
        )
    } else {
        user_text.to_owned()
    };

    if let Some(mode_prompt) = MODE_PROMPTS
        .iter()
        .find(|(id, _)| *id == mode_id)
        .map(|(_, p)| *p)
    {
        system_parts.push(mode_prompt.to_owned());
    }

    system_parts.push(SYSTEM_PROMPT.to_owned());

    if !active_cmd_sessions.is_empty() {
        let mut lines = vec!["Active terminal sessions:".to_owned()];
        for (sid, info) in active_cmd_sessions {
            let mut line = format!("- {sid}: cmd=`{}`", info.cmd);
            if info.running {
                line.push_str(" status=`running`");
            } else if let Some(exit_code) = info.exit_code {
                line.push_str(&format!(" status=`exited:{exit_code}`"));
            }
            if !info.last_output.is_empty() {
                let snippet = &info.last_output[info.last_output.len().saturating_sub(200)..];
                line.push_str(&format!(" last_output=`{}`", snippet.trim()));
            }
            lines.push(line);
        }
        system_parts.push(lines.join("\n"));
    }

    if interrupted {
        if let Some(task) = pending_task {
            system_parts.push(format!(
                "Previously interrupted task: {task}\nResume that exact task from the last relevant context. Do not restart from scratch or drift to unrelated files."
            ));
        }
    }

    if !pending_terminal_outputs.is_empty() {
        let mut lines = vec!["New terminal output since the last turn:".to_owned()];
        lines.extend_from_slice(pending_terminal_outputs);
        system_parts.push(lines.join("\n\n"));
    }

    if !system_parts.is_empty() {
        messages.push(ConversationMessage::new(
            "system",
            system_parts.join("\n\n"),
        ));
    }

    let recent = &turns[turns.len().saturating_sub(MAX_HISTORY_TURNS)..];
    for turn in recent {
        if turn.role == "system" {
            continue;
        }

        messages.push(ConversationMessage::new(
            turn.role.as_str(),
            truncate(&turn.content, MAX_TURN_CHARS),
        ));
    }

    messages.push(ConversationMessage::new("user", effective_user_text));
    messages
}

fn is_continue_prompt(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "continue" | "resume" | "go on" | "keep going"
    )
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let omitted = text.len() - limit;
    format!("{}\n... [truncated {omitted} chars]", &text[..limit])
}

fn tool_kind(name: &str) -> &'static str {
    TOOL_KINDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, k)| *k)
        .unwrap_or("other")
}

fn render_tool_title(name: &str, args: &Map<String, Value>) -> String {
    let path = str_arg(args, "path");
    let query = str_arg(args, "query");
    let url = str_arg(args, "url");
    let cmd = str_arg(args, "cmd");
    let session_id = str_arg(args, "session_id");
    let filename = str_arg(args, "filename");

    match name {
        "read_file_tool" => format!("Read {}", display_value(&path, ".")),
        "list_dir_tool" => format!("List {}", display_value(&path, ".")),
        "search_code_tool" => format!("Search {}", display_value(&query, "<empty>")),
        "web_search_tool" => format!("Search web {}", display_value(&query, "<empty>")),
        "web_fetch_tool" => format!("Fetch {}", display_value(&url, "<empty>")),
        "run_command_tool" => format!("Run {}", display_value(&cmd, "<empty>")),
        "list_command_sessions_tool" => "List command sessions".to_owned(),
        "start_command_session_tool" => {
            format!("Start command session {}", display_value(&cmd, "<empty>"))
        }
        "read_command_session_tool" => {
            format!(
                "Read command session {}",
                display_value(&session_id, "<empty>")
            )
        }
        "write_command_session_tool" => {
            format!(
                "Write command session {}",
                display_value(&session_id, "<empty>")
            )
        }
        "terminate_command_session_tool" => {
            format!(
                "Terminate command session {}",
                display_value(&session_id, "<empty>")
            )
        }
        "edit_file_tool" => format!("Edit {}", display_value(&path, "<empty>")),
        "patch_file_tool" => format!("Patch {}", display_value(&path, "<empty>")),
        "delete_path_tool" => format!("Delete {}", display_value(&path, "<empty>")),
        "create_artifact_tool" => format!("Create {}", display_value(&filename, "<auto>")),
        _ => name.to_owned(),
    }
}

fn display_value(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn str_arg(args: &Map<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

fn render_tool_started(name: &str, args: &Map<String, Value>) -> Option<String> {
    let path = str_arg(args, "path");
    let query = str_arg(args, "query");
    let url = str_arg(args, "url");
    let cmd = str_arg(args, "cmd");
    let session_id = str_arg(args, "session_id");
    let filename = str_arg(args, "filename");

    Some(match name {
        "read_file_tool" => format!("Reading `{path}`."),
        "list_dir_tool" => format!("Listing `{}`.", if path.is_empty() { "." } else { &path }),
        "search_code_tool" => format!("Searching for `{query}`."),
        "web_search_tool" => format!("Searching the web for `{query}`."),
        "web_fetch_tool" => format!("Fetching `{url}`."),
        "run_command_tool" => format!("Running `{cmd}`."),
        "list_command_sessions_tool" => "Listing terminal sessions.".to_owned(),
        "start_command_session_tool" => format!("Starting terminal session for `{cmd}`."),
        "read_command_session_tool" => format!("Reading terminal session `{session_id}`."),
        "write_command_session_tool" => format!("Writing to terminal session `{session_id}`."),
        "terminate_command_session_tool" => {
            format!("Terminating terminal session `{session_id}`.")
        }
        "edit_file_tool" => format!("Editing `{path}`."),
        "patch_file_tool" => format!("Patching `{path}`."),
        "delete_path_tool" => format!("Deleting `{path}`."),
        "create_artifact_tool" => {
            let fname = if filename.is_empty() {
                "<auto>".to_owned()
            } else {
                filename
            };
            format!("Creating `{fname}`.")
        }
        _ => format!("Running `{name}`."),
    })
}

fn render_tool_finished(name: &str, args: &Map<String, Value>, output: &str) -> Option<String> {
    let path = str_arg(args, "path");
    let query = str_arg(args, "query");
    let url = str_arg(args, "url");
    let cmd = str_arg(args, "cmd");
    let session_id = str_arg(args, "session_id");

    Some(match name {
        "read_file_tool" => append_tool_output(format!("Read `{path}`."), output, "text"),
        "list_dir_tool" => append_tool_output(
            format!("Listed `{}`.", if path.is_empty() { "." } else { &path }),
            output,
            "text",
        ),
        "search_code_tool" => {
            let matches = output.lines().count();
            append_tool_output(
                format!("Found {matches} lines for `{query}`."),
                output,
                "text",
            )
        }
        "web_search_tool" => {
            append_tool_output(format!("Searched the web for `{query}`."), output, "text")
        }
        "web_fetch_tool" => {
            let chars = output.len();
            append_tool_output(format!("Fetched `{url}` ({chars} chars)."), output, "text")
        }
        "run_command_tool" => {
            if let Some(session_id) = extract_result_line_field(output, "session_id") {
                if extract_result_line_field(output, "running").as_deref() == Some("true") {
                    append_tool_output(
                        format!("`{cmd}` is still running in session `{session_id}`."),
                        output,
                        "text",
                    )
                } else {
                    let exit = extract_exit_code(output);
                    let command_output = extract_command_tail(output, 30);
                    if command_output.is_empty() {
                        format!("`{cmd}` exited with code {exit}.")
                    } else {
                        format!("`{cmd}` exited with code {exit}.\n```text\n{command_output}\n```")
                    }
                }
            } else {
                let exit = extract_exit_code(output);
                let command_output = extract_command_tail(output, 30);
                if command_output.is_empty() {
                    format!("`{cmd}` exited with code {exit}.")
                } else {
                    format!("`{cmd}` exited with code {exit}.\n```text\n{command_output}\n```")
                }
            }
        }
        "list_command_sessions_tool" => {
            let count = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| v.as_array().map(|items| items.len()))
                .unwrap_or(0);
            append_tool_output(format!("Listed {count} terminal sessions."), output, "json")
        }
        "start_command_session_tool" => {
            // result is JSON with session_id
            let sid = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| {
                    v.get("session_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            append_tool_output(
                format!("Started terminal session `{sid}` for `{cmd}`."),
                output,
                "json",
            )
        }
        "read_command_session_tool" => {
            let chars = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| v.get("output").and_then(|v| v.as_str()).map(|s| s.len()))
                .unwrap_or(0);
            append_tool_output(
                format!("Read {chars} chars from `{session_id}`."),
                output,
                "text",
            )
        }
        "write_command_session_tool" => {
            let chars = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| v.get("written_chars").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            append_tool_output(
                format!("Wrote {chars} chars to `{session_id}`."),
                output,
                "json",
            )
        }
        "terminate_command_session_tool" => {
            let exit = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| v.get("exit_code").and_then(|v| v.as_i64()))
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            append_tool_output(
                format!("Terminated `{session_id}` (exit {exit})."),
                output,
                "json",
            )
        }
        "delete_path_tool" => append_tool_output(format!("Deleted `{path}`."), output, "json"),
        "create_artifact_tool" => {
            let filename = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| {
                    v.get("filename")
                        .or_else(|| v.get("path"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| str_arg(args, "filename"));
            if filename.is_empty() {
                append_tool_output("Created file.".to_owned(), output, "text")
            } else {
                append_tool_output(format!("Created `{filename}`."), output, "text")
            }
        }
        "edit_file_tool" => append_tool_output(format!("Updated `{path}`."), output, "text"),
        "patch_file_tool" => append_tool_output(format!("Patched `{path}`."), output, "diff"),
        _ => return None,
    })
}

fn render_tool_error(name: &str, args: &Map<String, Value>, error: &str) -> String {
    let summary = format!("{} failed.", render_tool_title(name, args));
    append_tool_output(summary, error, "text")
}

fn append_tool_output(summary: String, output: &str, language: &str) -> String {
    let preview = tool_output_preview(output);
    if preview.trim().is_empty() {
        return summary;
    }
    format!("{summary}\n```{language}\n{}\n```", escape_fence(&preview))
}

fn tool_output_preview(output: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        if let Some(preview) = value.get("preview").and_then(Value::as_str) {
            return truncate_tool_preview(preview, 6000);
        }
        if let Some(command_output) = value.get("output").and_then(Value::as_str) {
            return truncate_tool_preview(command_output, 6000);
        }
        if value.as_object().is_some_and(|object| object.is_empty()) {
            return String::new();
        }
        return truncate_tool_preview(&value.to_string(), 6000);
    }
    truncate_tool_preview(output, 6000)
}

fn truncate_tool_preview(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = 0;
    for (idx, _) in text.char_indices() {
        if idx > limit {
            break;
        }
        end = idx;
    }
    format!(
        "{}\n... [truncated {} chars]",
        &text[..end],
        text.len().saturating_sub(end)
    )
}

fn escape_fence(text: &str) -> String {
    text.replace("```", "'''")
}

/// Extract the last `max_lines` lines of the stdout section from run_command_tool output.
fn extract_stdout_tail(output: &str, max_lines: usize) -> String {
    // Format: "$ cmd\n\nexit_code: N\n\nstdout:\n{content}\n\nstderr:\n..."
    let stdout_start = match output.find("\nstdout:\n") {
        Some(pos) => pos + "\nstdout:\n".len(),
        None => return String::new(),
    };
    let stdout_end = output[stdout_start..]
        .find("\n\nstderr:")
        .map(|pos| stdout_start + pos)
        .unwrap_or(output.len());
    let stdout = output[stdout_start..stdout_end].trim();
    if stdout.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() <= max_lines {
        stdout.to_owned()
    } else {
        let skipped = lines.len() - max_lines;
        format!(
            "[... {} lines omitted ...]\n{}",
            skipped,
            lines[skipped..].join("\n")
        )
    }
}

fn extract_stderr_tail(output: &str, max_lines: usize) -> String {
    let stderr_start = match output.find("\nstderr:\n") {
        Some(pos) => pos + "\nstderr:\n".len(),
        None => return String::new(),
    };
    tail_lines(output[stderr_start..].trim(), max_lines)
}

fn extract_command_tail(output: &str, max_lines: usize) -> String {
    let stdout = extract_stdout_tail(output, max_lines);
    let stderr = extract_stderr_tail(output, max_lines);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("stdout:\n{stdout}"),
        (true, false) => format!("stderr:\n{stderr}"),
        (false, false) => format!("stdout:\n{stdout}\n\nstderr:\n{stderr}"),
    }
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_owned();
    }
    let skipped = lines.len() - max_lines;
    format!(
        "[... {} lines omitted ...]\n{}",
        skipped,
        lines[skipped..].join("\n")
    )
}

fn extract_exit_code(output: &str) -> String {
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("exit_code: ") {
            return rest.trim().to_owned();
        }
    }
    "unknown".to_owned()
}

fn extract_result_line_field(output: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    output.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|value| value.trim().to_owned())
    })
}

fn parse_tool_output(output: &str) -> Value {
    serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.to_owned()))
}

fn terminal_command_and_args(cmd: &str) -> (String, Vec<String>) {
    if contains_shell_metachar(cmd) {
        return ("sh".to_owned(), vec!["-c".to_owned(), cmd.to_owned()]);
    }
    match shell_words::split(cmd) {
        Ok(parts) if !parts.is_empty() => {
            let command = parts[0].clone();
            let args = parts[1..].to_vec();
            (command, args)
        }
        _ => ("sh".to_owned(), vec!["-c".to_owned(), cmd.to_owned()]),
    }
}

fn contains_shell_metachar(cmd: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    for ch in cmd.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '|' | '&' | ';' | '>' | '<' if !in_single && !in_double => return true,
            _ => {}
        }
    }
    false
}

fn extract_terminal_exit_code(value: &Value) -> Option<i64> {
    value
        .get("exitStatus")
        .and_then(|s| s.get("exitCode"))
        .and_then(|c| c.as_i64())
        .or_else(|| value.get("exitCode").and_then(|c| c.as_i64()))
        .or_else(|| {
            value
                .get("exit_status")
                .and_then(|s| s.get("exit_code"))
                .and_then(|c| c.as_i64())
        })
        .or_else(|| {
            value
                .get("status")
                .and_then(|s| s.get("exitCode"))
                .and_then(|c| c.as_i64())
        })
}

fn extract_terminal_output(value: &Value) -> String {
    if let Some(s) = value.get("output").and_then(|v| v.as_str()) {
        return s.to_owned();
    }
    if let Some(s) = value.get("content").and_then(|v| v.as_str()) {
        return s.to_owned();
    }
    if let Some(items) = value.get("content").and_then(|v| v.as_array()) {
        let joined = items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("output").and_then(|v| v.as_str()))
                    .or_else(|| item.as_str())
            })
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return joined;
        }
    }
    value
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::fs as stdfs;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::sync::{Mutex as AsyncMutex, mpsc};

    use super::{
        AcpProgressSink, ClientCaller, ProgressRegistry, build_messages, is_continue_prompt,
        parse_tool_output, render_tool_finished,
    };
    use crate::agent_loop::{ModelClient, ToolExecutor};
    use crate::mlx_client::ChatMessage;
    use crate::session_store::CommandSessionInfo;
    use crate::tools::BuiltinToolRegistry;

    struct MockModel {
        responses: AsyncMutex<VecDeque<String>>,
    }

    impl MockModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: AsyncMutex::new(
                    responses
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<VecDeque<_>>(),
                ),
            }
        }
    }

    #[async_trait]
    impl ModelClient for MockModel {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
            _max_tokens: u32,
            _temperature: f32,
            _think_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        ) -> Result<crate::mlx_client::CompletionResult> {
            let text = self
                .responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no mock responses left"))?;
            Ok(crate::mlx_client::CompletionResult {
                content: Some(text),
                tool_calls: Vec::new(),
            })
        }
    }

    fn drain_updates(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<Value> {
        let mut values = Vec::new();
        while let Ok(message) = rx.try_recv() {
            values.push(serde_json::from_str(&message).expect("valid json-rpc message"));
        }
        values
    }

    fn thought_texts(updates: &[Value]) -> Vec<String> {
        updates
            .iter()
            .filter_map(|msg| {
                msg.get("params")
                    .and_then(|v| v.get("update"))
                    .and_then(|v| v.get("sessionUpdate"))
                    .and_then(|v| v.as_str())
                    .filter(|kind| *kind == "agent_thought_chunk")?;
                msg.get("params")
                    .and_then(|v| v.get("update"))
                    .and_then(|v| v.get("content"))
                    .and_then(|v| v.get("text"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn render_tool_finished_reports_file_editing_tools() {
        let args = json!({"path": "notes.txt"}).as_object().cloned().unwrap();
        assert_eq!(
            render_tool_finished("edit_file_tool", &args, "{}"),
            Some("Updated `notes.txt`.".to_owned())
        );
        assert_eq!(
            render_tool_finished("patch_file_tool", &args, "{}"),
            Some("Patched `notes.txt`.".to_owned())
        );
        assert_eq!(
            render_tool_finished(
                "create_artifact_tool",
                &json!({}).as_object().cloned().unwrap(),
                "{}"
            ),
            Some("Created file.".to_owned())
        );
        assert_eq!(
            render_tool_finished("delete_path_tool", &args, "{}"),
            Some("Deleted `notes.txt`.".to_owned())
        );
    }

    #[test]
    fn render_tool_finished_reports_running_run_command_sessions() {
        let args = json!({"cmd": "tail -f log.txt"})
            .as_object()
            .cloned()
            .unwrap();
        let output = "$ tail -f log.txt\n\nsession_id: cmdsess_abc123\n\nrunning: true\n\nstdout:\nready\n\nstderr:\n\n[command is still running in session `cmdsess_abc123`; use read_command_session_tool to follow it or terminate_command_session_tool to stop it]";
        let rendered = render_tool_finished("run_command_tool", &args, output).expect("rendered");
        assert!(
            rendered.contains("`tail -f log.txt` is still running in session `cmdsess_abc123`.")
        );
        assert!(rendered.contains("stdout:"));
        assert!(rendered.contains("ready"));
    }

    #[test]
    fn parse_tool_output_prefers_json() {
        assert_eq!(
            parse_tool_output(r#"{"status":"ok"}"#),
            json!({"status": "ok"})
        );
        assert_eq!(
            parse_tool_output("plain text"),
            Value::String("plain text".to_owned())
        );
    }

    #[test]
    fn terminal_command_and_args_splits_plain_command() {
        let (command, args) = super::terminal_command_and_args("cargo test --verbose");
        assert_eq!(command, "cargo");
        assert_eq!(args, vec!["test".to_owned(), "--verbose".to_owned()]);
    }

    #[test]
    fn terminal_command_and_args_falls_back_for_shell_syntax() {
        let (command, args) = super::terminal_command_and_args("cargo test 2>&1");
        assert_eq!(command, "sh");
        assert_eq!(args, vec!["-c".to_owned(), "cargo test 2>&1".to_owned()]);
    }

    #[test]
    fn extract_terminal_output_accepts_multiple_shapes() {
        assert_eq!(
            super::extract_terminal_output(&json!({"output": "line1\nline2"})),
            "line1\nline2".to_owned()
        );
        assert_eq!(
            super::extract_terminal_output(&json!({"content": "lineA"})),
            "lineA".to_owned()
        );
        assert_eq!(
            super::extract_terminal_output(
                &json!({"content": [{"text": "hello"}, {"output": " world"}]})
            ),
            "hello world".to_owned()
        );
    }

    #[test]
    fn detects_continue_prompts() {
        assert!(is_continue_prompt("continue"));
        assert!(is_continue_prompt(" Continue "));
        assert!(is_continue_prompt("resume"));
        assert!(!is_continue_prompt("continue with a new task"));
        assert!(!is_continue_prompt("fix the routes bug"));
    }

    #[test]
    fn build_messages_rewrites_bare_continue_to_pending_task() {
        let messages = build_messages(
            "agent",
            &[],
            true,
            Some("trace the ACP routes"),
            &std::collections::HashMap::new(),
            &[],
            "continue",
        );

        assert!(messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .contains("Previously interrupted task: trace the ACP routes")
        }));
        assert_eq!(
            messages.last().map(|m| m.content.as_str()),
            Some("Continue the previously interrupted task: trace the ACP routes")
        );
    }

    #[test]
    fn hydrates_running_command_sessions_from_persisted_metadata() {
        let tempdir = TempDir::new().expect("tempdir");
        crate::tools::command::with_test_sessions_dir(tempdir.path(), || {
            let sessions = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            crate::tools::command::start_command_session(
                &sessions,
                tempdir.path(),
                "printf 'ready\\n'; sleep 2",
            )
            .expect("start");

            let mut state =
                crate::session_store::new_session(tempdir.path().to_str().expect("utf8 cwd"));
            super::hydrate_command_sessions_for_cwd(&mut state);

            assert_eq!(state.active_command_sessions.len(), 1);
            let info = state.active_command_sessions.values().next().unwrap();
            assert_eq!(info.cmd, "printf 'ready\\n'; sleep 2");
            assert!(info.running);
        });
    }

    #[test]
    fn poll_active_command_sessions_collects_new_output_and_updates_state() {
        let tempdir = TempDir::new().expect("tempdir");
        crate::tools::command::with_test_sessions_dir(tempdir.path(), || {
            let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
            let started = crate::tools::command::start_command_session(
                &registry.cmd_sessions,
                tempdir.path(),
                "printf 'ready\\n'",
            )
            .expect("start");
            let session_id = started["session_id"].as_str().unwrap().to_owned();

            let mut state =
                crate::session_store::new_session(tempdir.path().to_str().expect("utf8 cwd"));
            state.active_command_sessions.insert(
                session_id.clone(),
                CommandSessionInfo {
                    session_id: session_id.clone(),
                    cmd: "printf 'ready\\n'".to_owned(),
                    last_output: String::new(),
                    running: true,
                    exit_code: None,
                },
            );

            std::thread::sleep(std::time::Duration::from_millis(150));
            let outputs = super::poll_active_command_sessions(&registry, &mut state);

            assert!(outputs.iter().any(|output| output.contains("ready")));
            assert!(!state.active_command_sessions.contains_key(&session_id));
        });
    }

    #[tokio::test]
    async fn progress_registry_emits_create_artifact_reasoning_and_tool_output() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec![
            "markdown",
            "summary.md",
            "# Summary\nCreated by the tool.\n",
        ]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (caller_tx, _caller_rx) = mpsc::unbounded_channel();
        let progress_registry = ProgressRegistry {
            inner: BuiltinToolRegistry::new_with_model(tempdir.path(), model)
                .expect("registry")
                .with_progress_sink(Arc::new(AcpProgressSink {
                    tx: tx.clone(),
                    session_id: "sess_test".to_owned(),
                })),
            tx,
            session_id: "sess_test".to_owned(),
            counter: Arc::new(AtomicU64::new(0)),
            caller: ClientCaller::new(caller_tx),
            has_terminal: false,
            active_reasoning: Arc::new(std::sync::Mutex::new(None)),
            terminal_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };

        progress_registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "Write a short markdown summary file."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let updates = drain_updates(&mut rx);
        let thoughts = thought_texts(&updates);
        assert!(
            thoughts
                .iter()
                .any(|t| t.contains("Planning a new artifact before generating file contents."))
        );
        assert!(
            thoughts
                .iter()
                .any(|t| t.contains("Generating `summary.md` as markdown."))
        );
        assert!(
            thoughts
                .iter()
                .any(|t| t.contains("Created `") && t.contains("summary.md`."))
        );

        let update = updates
            .iter()
            .find(|msg| {
                msg.get("params")
                    .and_then(|v| v.get("update"))
                    .and_then(|v| v.get("sessionUpdate"))
                    .and_then(|v| v.as_str())
                    == Some("tool_call_update")
            })
            .expect("tool_call_update");
        assert_eq!(
            update["params"]["update"]["rawOutput"]["filename"],
            Value::String("summary.md".to_owned())
        );
        let update_text = update["params"]["update"]["content"][0]["content"]["text"]
            .as_str()
            .expect("update text");
        assert!(update_text.contains("Created `summary.md`."));
        assert!(update_text.contains("# Summary"));
    }

    #[tokio::test]
    async fn progress_registry_keeps_reasoning_out_of_tool_call_content() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("notes.txt"), "old").expect("write notes");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (caller_tx, _caller_rx) = mpsc::unbounded_channel();
        let active_reasoning = Arc::new(std::sync::Mutex::new(Some(
            "I should patch the file now.".to_owned(),
        )));
        let progress_registry = ProgressRegistry {
            inner: BuiltinToolRegistry::new(tempdir.path()).expect("registry"),
            tx,
            session_id: "sess_test".to_owned(),
            counter: Arc::new(AtomicU64::new(0)),
            caller: ClientCaller::new(caller_tx),
            has_terminal: false,
            active_reasoning,
            terminal_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };

        progress_registry
            .invoke(
                "patch_file_tool",
                json!({"path": "notes.txt", "old_text": "old", "new_text": "new"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let updates = drain_updates(&mut rx);
        let tool_call = updates
            .iter()
            .find(|msg| {
                msg.get("params")
                    .and_then(|v| v.get("update"))
                    .and_then(|v| v.get("sessionUpdate"))
                    .and_then(|v| v.as_str())
                    == Some("tool_call")
            })
            .expect("tool_call");
        assert_eq!(
            tool_call["params"]["update"]["content"][0]["content"]["text"],
            Value::String("Patching `notes.txt`.".to_owned())
        );

        let finished = updates
            .iter()
            .filter(|msg| {
                msg.get("params")
                    .and_then(|v| v.get("update"))
                    .and_then(|v| v.get("sessionUpdate"))
                    .and_then(|v| v.as_str())
                    == Some("tool_call_update")
            })
            .last()
            .expect("tool_call_update");
        let finished_text = finished["params"]["update"]["content"][0]["content"]["text"]
            .as_str()
            .expect("finished text");
        assert!(finished_text.contains("Patched `notes.txt`."));
        assert!(finished_text.contains("--- old"));
        assert!(finished_text.contains("+++ new"));
    }

    #[tokio::test]
    async fn progress_registry_emits_file_modified_for_edit_without_completed_thought() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("notes.txt"), "original content\n").expect("write notes");
        let model = Arc::new(MockModel::new(vec!["updated content\n"]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (caller_tx, _caller_rx) = mpsc::unbounded_channel();
        let progress_registry = ProgressRegistry {
            inner: BuiltinToolRegistry::new_with_model(tempdir.path(), model)
                .expect("registry")
                .with_progress_sink(Arc::new(AcpProgressSink {
                    tx: tx.clone(),
                    session_id: "sess_test".to_owned(),
                })),
            tx,
            session_id: "sess_test".to_owned(),
            counter: Arc::new(AtomicU64::new(0)),
            caller: ClientCaller::new(caller_tx),
            has_terminal: false,
            active_reasoning: Arc::new(std::sync::Mutex::new(None)),
            terminal_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };

        progress_registry
            .invoke(
                "edit_file_tool",
                json!({"path": "notes.txt", "instruction": "Replace the contents."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let thoughts = thought_texts(&drain_updates(&mut rx));
        assert!(
            thoughts
                .iter()
                .any(|t| t.contains("Updated `") && t.contains("notes.txt`."))
        );
        assert!(!thoughts.iter().any(|t| t.trim() == "Completed."));
    }
}
