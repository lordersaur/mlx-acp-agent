/// Session store — mirrors sessions.py.
///
/// Tracks conversation history, active command sessions, and interruption
/// state for each ACP session.  Phase 6 adds disk persistence so chat
/// history survives editor restarts.
use std::collections::HashMap;
use std::fs;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::warn;
use uuid::Uuid;

use crate::agent_loop::ToolExecution;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_LAST_OUTPUT_CHARS: usize = 1200;

/// Maximum number of turns to persist to disk.
const MAX_PERSISTED_TURNS: usize = 40;

/// Maximum byte size per turn content when persisting (4 KiB).
const MAX_PERSISTED_TURN_CONTENT: usize = 4096;

const VALIDATION_TERMS: &[&str] = &[
    "test",
    "pytest",
    "unittest",
    "compile",
    "py_compile",
    "build",
    "lint",
    "check",
    "mypy",
    "ruff",
    "eslint",
    "tsc",
    "cargo test",
    "go test",
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandSessionInfo {
    pub session_id: String,
    pub cmd: String,
    pub last_output: String,
    pub running: bool,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnRecord {
    pub role: String,
    pub content: String,
    pub tools_used: Vec<String>,
    pub files_read: Vec<String>,
    pub files_changed: Vec<FileChange>,
    pub commands_run: Vec<String>,
    pub command_sessions_touched: Vec<String>,
    pub validation_results: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: String,
    pub cwd: String,
    pub mode_id: String,
    pub turns: Vec<TurnRecord>,
    pub active_command_sessions: HashMap<String, CommandSessionInfo>,
    pub interrupted: bool,
    pub pending_task: Option<String>,
    /// UI-only: maps event key → external tool-call id
    pub active_tool_calls: HashMap<String, String>,
    pub tool_event_counter: u64,
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

pub fn new_session(cwd: &str) -> SessionState {
    SessionState {
        session_id: format!("sess_{}", Uuid::new_v4().simple()),
        cwd: cwd.to_owned(),
        mode_id: "agent".to_owned(),
        turns: Vec::new(),
        active_command_sessions: HashMap::new(),
        interrupted: false,
        pending_task: None,
        active_tool_calls: HashMap::new(),
        tool_event_counter: 0,
    }
}

// ---------------------------------------------------------------------------
// build_turn_record — mirrors sessions.build_turn_record
// ---------------------------------------------------------------------------

pub fn build_turn_record(role: &str, content: &str, tool_results: &[ToolExecution]) -> TurnRecord {
    let mut record = TurnRecord {
        role: role.to_owned(),
        content: content.to_owned(),
        ..Default::default()
    };

    let mut seen_tools: std::collections::HashSet<String> = Default::default();

    for tr in tool_results {
        if seen_tools.insert(tr.name.clone()) {
            record.tools_used.push(tr.name.clone());
        }

        match tr.name.as_str() {
            "read_file_tool" => {
                let path = string_arg(&tr.arguments, "path");
                if !path.is_empty() && !record.files_read.contains(&path) {
                    record.files_read.push(path);
                }
            }
            "create_artifact_tool" | "edit_file_tool" | "patch_file_tool" | "delete_path_tool" => {
                extract_file_change(&mut record, &tr.result);
            }
            "run_command_tool" => {
                let cmd = string_arg(&tr.arguments, "cmd");
                if !cmd.is_empty() {
                    record.commands_run.push(cmd.clone());
                    if VALIDATION_TERMS
                        .iter()
                        .any(|term| cmd.to_lowercase().contains(term))
                    {
                        let exit = extract_exit_code(&tr.result);
                        record.validation_results.push(format!("{cmd}: {exit}"));
                    }
                }
            }
            "start_command_session_tool" => {
                let cmd = string_arg(&tr.arguments, "cmd");
                if !cmd.is_empty() {
                    record.commands_run.push(cmd);
                }
                let sid = json_field(&tr.result, "session_id");
                if !sid.is_empty() {
                    record.command_sessions_touched.push(sid);
                }
            }
            "read_command_session_tool"
            | "write_command_session_tool"
            | "terminate_command_session_tool" => {
                let sid = string_arg(&tr.arguments, "session_id");
                let sid = if sid.is_empty() {
                    json_field(&tr.result, "session_id")
                } else {
                    sid
                };
                if !sid.is_empty() && !record.command_sessions_touched.contains(&sid) {
                    record.command_sessions_touched.push(sid);
                }
            }
            _ => {}
        }
    }

    record
}

// ---------------------------------------------------------------------------
// update_command_sessions — mirrors sessions.update_command_sessions
// ---------------------------------------------------------------------------

pub fn update_command_sessions(state: &mut SessionState, tool_results: &[ToolExecution]) {
    for tr in tool_results {
        match tr.name.as_str() {
            "run_command_tool" => handle_run_command(state, tr),
            "start_command_session_tool" => handle_session_start(state, tr),
            "read_command_session_tool" => handle_session_read(state, tr),
            "terminate_command_session_tool" => handle_session_terminate(state, tr),
            _ => {}
        }
    }
}

fn handle_session_start(state: &mut SessionState, tr: &ToolExecution) {
    let payload: Value = match serde_json::from_str(&tr.result) {
        Ok(v) => v,
        Err(_) => return,
    };
    let sid = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    if sid.is_empty() {
        return;
    }
    let cmd = payload
        .get("cmd")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| string_arg(&tr.arguments, "cmd"));
    state.active_command_sessions.insert(
        sid.clone(),
        CommandSessionInfo {
            session_id: sid,
            cmd,
            last_output: String::new(),
            running: true,
            exit_code: None,
        },
    );
}

fn handle_run_command(state: &mut SessionState, tr: &ToolExecution) {
    let sid = result_line_field(&tr.result, "session_id");
    if sid.is_empty() {
        return;
    }

    let running = result_line_field(&tr.result, "running") == "true";
    if !running {
        state.active_command_sessions.remove(&sid);
        return;
    }

    let cmd = string_arg(&tr.arguments, "cmd");
    state.active_command_sessions.insert(
        sid.clone(),
        CommandSessionInfo {
            session_id: sid,
            cmd,
            last_output: extract_stdout_block(&tr.result),
            running: true,
            exit_code: None,
        },
    );
}

fn handle_session_read(state: &mut SessionState, tr: &ToolExecution) {
    let sid = string_arg(&tr.arguments, "session_id");
    if sid.is_empty() {
        return;
    }
    let payload: Value = match serde_json::from_str(&tr.result) {
        Ok(v) => v,
        Err(_) => return,
    };
    let output = payload
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let running = payload.get("running").and_then(|v| v.as_bool());

    if let Some(info) = state.active_command_sessions.get_mut(&sid) {
        if !output.is_empty() {
            let tail = output
                .char_indices()
                .rev()
                .take(MAX_LAST_OUTPUT_CHARS)
                .last()
                .map(|(i, _)| &output[i..])
                .unwrap_or(&output[output.len().saturating_sub(MAX_LAST_OUTPUT_CHARS)..]);
            info.last_output = tail.to_owned();
        }
        info.running = running.unwrap_or(true);
        info.exit_code = payload.get("exit_code").and_then(|v| v.as_i64());
    }

    if running == Some(false) {
        state.active_command_sessions.remove(&sid);
    }
}

fn handle_session_terminate(state: &mut SessionState, tr: &ToolExecution) {
    let sid = string_arg(&tr.arguments, "session_id");
    state.active_command_sessions.remove(&sid);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn string_arg(arguments: &Map<String, Value>, key: &str) -> String {
    arguments
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

fn json_field(text: &str, field: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get(field).and_then(|f| f.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

fn extract_file_change(record: &mut TurnRecord, result: &str) {
    let payload: Value = match serde_json::from_str(result) {
        Ok(v) => v,
        Err(_) => return,
    };
    let path = payload
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let status = payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("changed")
        .to_owned();
    if !path.is_empty() {
        record.files_changed.push(FileChange { path, status });
    }
}

fn extract_exit_code(result: &str) -> String {
    for line in result.lines() {
        if let Some(rest) = line.strip_prefix("exit_code: ") {
            return format!("exit {}", rest.trim());
        }
    }
    "exit unknown".to_owned()
}

fn result_line_field(result: &str, key: &str) -> String {
    let prefix = format!("{key}: ");
    for line in result.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            return rest.trim().to_owned();
        }
    }
    String::new()
}

fn extract_stdout_block(result: &str) -> String {
    let Some(start) = result.find("\n\nstdout:\n") else {
        return String::new();
    };
    let start = start + "\n\nstdout:\n".len();
    let end = result[start..]
        .find("\n\nstderr:\n")
        .map(|offset| start + offset)
        .unwrap_or(result.len());
    result[start..end].trim().to_owned()
}

// ---------------------------------------------------------------------------
// Persisted session history (Phase 6)
// ---------------------------------------------------------------------------

/// On-disk representation of a session's durable state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub session_id: String,
    pub cwd: String,
    pub mode_id: String,
    pub turns: Vec<TurnRecord>,
    pub interrupted: bool,
    pub pending_task: Option<String>,
    /// Snapshot of active command sessions at persist time so we can show
    /// context on reload (actual process liveness is re-checked separately).
    pub active_command_sessions: HashMap<String, CommandSessionInfo>,
}

#[cfg(test)]
static TEST_HISTORY_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
static TEST_HISTORY_DIR_LOCK: Mutex<()> = Mutex::new(());

fn history_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(path) = TEST_HISTORY_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return path;
        }
    }

    if let Ok(path) = std::env::var("MLX_ACP_HISTORY_DIR") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".mlx-acp-agent")
        .join("history")
}

fn history_path(session_id: &str) -> PathBuf {
    history_dir().join(format!("{session_id}.json"))
}

/// Persist the durable subset of a session to disk.
pub fn persist_session(state: &SessionState) {
    let mut turns: Vec<TurnRecord> = state.turns.clone();

    // Truncate to bounded size.
    if turns.len() > MAX_PERSISTED_TURNS {
        let drain_to = turns.len() - MAX_PERSISTED_TURNS;
        turns.drain(..drain_to);
    }

    // Trim individual turn content to keep the file small.
    for turn in &mut turns {
        if turn.content.len() > MAX_PERSISTED_TURN_CONTENT {
            turn.content.truncate(MAX_PERSISTED_TURN_CONTENT);
            turn.content.push_str("\n... [truncated]");
        }
    }

    let persisted = PersistedSession {
        session_id: state.session_id.clone(),
        cwd: state.cwd.clone(),
        mode_id: state.mode_id.clone(),
        turns,
        interrupted: state.interrupted,
        pending_task: state.pending_task.clone(),
        active_command_sessions: state.active_command_sessions.clone(),
    };

    let dir = history_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        warn!("failed to create history dir {}: {e}", dir.display());
        return;
    }
    let path = history_path(&state.session_id);
    match serde_json::to_string_pretty(&persisted) {
        Ok(json) => {
            if let Err(e) = fs::write(&path, json) {
                warn!("failed to write history {}: {e}", path.display());
            }
        }
        Err(e) => warn!("failed to serialize history: {e}"),
    }
}

/// Try to load a previously persisted session from disk.  Returns `None` if
/// the file does not exist or cannot be parsed.
pub fn load_persisted_session(session_id: &str) -> Option<PersistedSession> {
    let path = history_path(session_id);
    let data = fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<PersistedSession>(&data) {
        Ok(persisted) => Some(persisted),
        Err(e) => {
            warn!("failed to parse persisted session {}: {e}", path.display());
            None
        }
    }
}

/// Restore a `SessionState` from a `PersistedSession`, merging with the
/// provided `cwd` (the editor may have moved).
pub fn restore_session(persisted: PersistedSession, cwd: &str) -> SessionState {
    SessionState {
        session_id: persisted.session_id,
        cwd: cwd.to_owned(),
        mode_id: persisted.mode_id,
        turns: persisted.turns,
        active_command_sessions: persisted.active_command_sessions,
        interrupted: persisted.interrupted,
        pending_task: persisted.pending_task,
        active_tool_calls: HashMap::new(),
        tool_event_counter: 0,
    }
}

/// List all persisted session IDs (used for diagnostics / cleanup).
pub fn list_persisted_sessions() -> Vec<String> {
    let dir = history_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".json").map(str::to_owned)
        })
        .collect()
}

/// Remove persisted history files older than `max_age`.
pub fn prune_old_history(max_age: std::time::Duration) {
    let dir = history_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() > max_age {
            fs::remove_file(entry.path()).ok();
        }
    }
}

#[cfg(test)]
pub(crate) fn set_test_history_dir(path: &Path) {
    *TEST_HISTORY_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
}

#[cfg(test)]
pub(crate) fn clear_test_history_dir() {
    *TEST_HISTORY_DIR.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
pub(crate) fn with_test_history_dir<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = TEST_HISTORY_DIR_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    set_test_history_dir(path);
    let result = f();
    clear_test_history_dir();
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{build_turn_record, new_session, update_command_sessions};
    use crate::agent_loop::ToolExecution;

    fn tool_exec(name: &str, args: serde_json::Value, result: &str) -> ToolExecution {
        ToolExecution {
            id: String::new(),
            name: name.to_owned(),
            arguments: args.as_object().cloned().unwrap_or_default(),
            result: result.to_owned(),
            error: false,
        }
    }

    #[test]
    fn build_turn_record_tracks_reads_and_writes() {
        let results = vec![
            tool_exec(
                "read_file_tool",
                json!({"path": "src/main.rs"}),
                "fn main() {}",
            ),
            tool_exec(
                "patch_file_tool",
                json!({"path": "src/main.rs", "old_text": "a", "new_text": "b"}),
                r#"{"status":"patched","path":"/abs/src/main.rs","replace_all":false}"#,
            ),
            tool_exec(
                "run_command_tool",
                json!({"cmd": "cargo test"}),
                "exit_code: 0\nstdout:\ntest passed",
            ),
        ];

        let record = build_turn_record("assistant", "done", &results);
        assert_eq!(record.files_read, vec!["src/main.rs"]);
        assert_eq!(record.files_changed[0].path, "/abs/src/main.rs");
        assert_eq!(record.commands_run, vec!["cargo test"]);
        assert_eq!(record.validation_results, vec!["cargo test: exit 0"]);
    }

    #[test]
    fn update_command_sessions_tracks_start_and_terminate() {
        let mut state = new_session("/tmp");
        let start = tool_exec(
            "start_command_session_tool",
            json!({"cmd": "tail -f log.txt"}),
            r#"{"session_id":"cmdsess_abc","cmd":"tail -f log.txt","running":true}"#,
        );
        update_command_sessions(&mut state, &[start]);
        assert!(state.active_command_sessions.contains_key("cmdsess_abc"));

        let terminate = tool_exec(
            "terminate_command_session_tool",
            json!({"session_id": "cmdsess_abc"}),
            r#"{"session_id":"cmdsess_abc","running":false,"exit_code":0}"#,
        );
        update_command_sessions(&mut state, &[terminate]);
        assert!(!state.active_command_sessions.contains_key("cmdsess_abc"));
    }

    #[test]
    fn update_command_sessions_tracks_long_running_run_command_sessions() {
        let mut state = new_session("/tmp");
        let running = tool_exec(
            "run_command_tool",
            json!({"cmd": "for i in 1 2 3; do echo tick; sleep 5; done"}),
            "$ for i in 1 2 3; do echo tick; sleep 5; done\n\nsession_id: cmdsess_run123\n\nrunning: true\n\nstdout:\ntick\n\nstderr:\n\n[command is still running in session `cmdsess_run123`; use read_command_session_tool to follow it or terminate_command_session_tool to stop it]",
        );

        update_command_sessions(&mut state, &[running]);
        let info = state
            .active_command_sessions
            .get("cmdsess_run123")
            .expect("tracked session");
        assert_eq!(info.cmd, "for i in 1 2 3; do echo tick; sleep 5; done");
        assert_eq!(info.last_output, "tick");
        assert!(info.running);
    }

    // -----------------------------------------------------------------------
    // Phase 6: persistence tests
    // -----------------------------------------------------------------------

    #[test]
    fn persist_and_reload_session_round_trips() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            let mut state = new_session("/projects/foo");
            state.turns.push(build_turn_record("user", "hello", &[]));
            state
                .turns
                .push(build_turn_record("assistant", "Hi! How can I help?", &[]));
            state.pending_task = Some("fix the bug".to_owned());
            state.interrupted = true;

            super::persist_session(&state);

            let loaded = super::load_persisted_session(&state.session_id)
                .expect("should load persisted session");
            assert_eq!(loaded.session_id, state.session_id);
            assert_eq!(loaded.turns.len(), 2);
            assert_eq!(loaded.turns[0].role, "user");
            assert_eq!(loaded.turns[0].content, "hello");
            assert_eq!(loaded.turns[1].role, "assistant");
            assert!(loaded.interrupted);
            assert_eq!(loaded.pending_task.as_deref(), Some("fix the bug"));
        });
    }

    #[test]
    fn persist_truncates_old_turns() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            let mut state = new_session("/tmp");
            for i in 0..60 {
                state.turns.push(build_turn_record(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &format!("turn {i}"),
                    &[],
                ));
            }

            super::persist_session(&state);

            let loaded = super::load_persisted_session(&state.session_id).expect("should load");
            assert_eq!(loaded.turns.len(), super::MAX_PERSISTED_TURNS);
            // Should keep the most recent turns
            assert_eq!(loaded.turns[0].content, "turn 20");
            assert_eq!(loaded.turns.last().unwrap().content, "turn 59");
        });
    }

    #[test]
    fn persist_truncates_large_turn_content() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            let mut state = new_session("/tmp");
            let big_content = "x".repeat(10_000);
            state
                .turns
                .push(build_turn_record("user", &big_content, &[]));

            super::persist_session(&state);

            let loaded = super::load_persisted_session(&state.session_id).expect("should load");
            assert!(loaded.turns[0].content.len() < 5000);
            assert!(loaded.turns[0].content.ends_with("... [truncated]"));
        });
    }

    #[test]
    fn restore_session_uses_new_cwd() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            let mut state = new_session("/old/path");
            state.turns.push(build_turn_record("user", "test", &[]));
            super::persist_session(&state);

            let persisted = super::load_persisted_session(&state.session_id).expect("should load");
            let restored = super::restore_session(persisted, "/new/path");
            assert_eq!(restored.cwd, "/new/path");
            assert_eq!(restored.session_id, state.session_id);
            assert_eq!(restored.turns.len(), 1);
        });
    }

    #[test]
    fn load_nonexistent_session_returns_none() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            assert!(super::load_persisted_session("sess_doesnotexist").is_none());
        });
    }

    #[test]
    fn persist_includes_active_command_sessions() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            let mut state = new_session("/tmp");
            state.active_command_sessions.insert(
                "cmdsess_abc".to_owned(),
                super::CommandSessionInfo {
                    session_id: "cmdsess_abc".to_owned(),
                    cmd: "tail -f log.txt".to_owned(),
                    last_output: "ready".to_owned(),
                    running: true,
                    exit_code: None,
                },
            );

            super::persist_session(&state);

            let loaded = super::load_persisted_session(&state.session_id).expect("should load");
            assert_eq!(loaded.active_command_sessions.len(), 1);
            let info = loaded.active_command_sessions.get("cmdsess_abc").unwrap();
            assert_eq!(info.cmd, "tail -f log.txt");
        });
    }

    #[test]
    fn list_persisted_sessions_finds_saved_sessions() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            let s1 = new_session("/a");
            let s2 = new_session("/b");
            super::persist_session(&s1);
            super::persist_session(&s2);

            let ids = super::list_persisted_sessions();
            assert!(ids.contains(&s1.session_id));
            assert!(ids.contains(&s2.session_id));
        });
    }

    #[test]
    fn interrupted_task_survives_restart() {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        super::with_test_history_dir(tempdir.path(), || {
            let mut state = new_session("/projects/myapp");
            // Simulate a multi-turn conversation
            state
                .turns
                .push(build_turn_record("user", "refactor the auth module", &[]));
            state.turns.push(build_turn_record(
                "assistant",
                "I'll start by reading the auth module.",
                &[tool_exec(
                    "read_file_tool",
                    json!({"path": "src/auth.rs"}),
                    "pub fn authenticate() {}",
                )],
            ));
            // Task was interrupted mid-way
            state.interrupted = true;
            state.pending_task = Some("refactor the auth module".to_owned());

            super::persist_session(&state);

            // "Restart" — load from disk into a fresh session
            let persisted = super::load_persisted_session(&state.session_id).expect("should load");
            let restored = super::restore_session(persisted, "/projects/myapp");

            assert!(restored.interrupted);
            assert_eq!(
                restored.pending_task.as_deref(),
                Some("refactor the auth module")
            );
            assert_eq!(restored.turns.len(), 2);
            assert_eq!(restored.turns[1].files_read, vec!["src/auth.rs"]);
        });
    }
}
