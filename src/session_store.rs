/// Session store — mirrors sessions.py.
///
/// Tracks conversation history and active command sessions for each ACP
/// session. Session data stays in memory for the life of the process.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::warn;
use uuid::Uuid;

use crate::agent_loop::ToolExecution;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_LAST_OUTPUT_CHARS: usize = 1200;

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
        Err(e) => {
            warn!("failed to parse session read payload: {e}");
            return;
        }
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
    let combined_output = extract_command_output(&tr.result);
    state.active_command_sessions.insert(
        sid.clone(),
        CommandSessionInfo {
            session_id: sid,
            cmd,
            last_output: combined_output,
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
        Err(e) => {
            warn!("failed to parse session read payload: {e}");
            return;
        }
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
        Err(e) => {
            warn!("failed to parse file change payload: {e}");
            return;
        }
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

fn extract_stderr_block(result: &str) -> String {
    let Some(start) = result.find("\n\nstderr:\n") else {
        return String::new();
    };
    let start = start + "\n\nstderr:\n".len();
    let tail = &result[start..];
    let end = [
        "\n\n[command is still running",
        "\n[command is still running",
    ]
    .iter()
    .filter_map(|marker| tail.find(marker).map(|offset| start + offset))
    .min()
    .unwrap_or(result.len());
    result[start..end].trim().to_owned()
}

fn extract_command_output(result: &str) -> String {
    let stdout = extract_stdout_block(result);
    let stderr = extract_stderr_block(result);

    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => format!("stderr:\n{stderr}"),
        (false, false) => format!("{stdout}\n\nstderr:\n{stderr}"),
    }
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
}
