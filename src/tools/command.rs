use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

pub const MAX_COMMAND_OUTPUT_CHARS: usize = 12000;
const COMPACT_THRESHOLD: usize = 65536;
const MAX_PERSISTED_OUTPUT_CHARS: usize = 1200;

#[cfg(test)]
static TEST_SESSIONS_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
#[cfg(test)]
static TEST_SESSIONS_DIR_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// run_command
// ---------------------------------------------------------------------------

const RUN_COMMAND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Command session types
// ---------------------------------------------------------------------------

struct CmdBuffer {
    data: String,
    read_offset: usize,
    closed: bool,
}

impl CmdBuffer {
    fn new() -> Self {
        Self {
            data: String::new(),
            read_offset: 0,
            closed: false,
        }
    }

    fn push(&mut self, text: &str) {
        self.data.push_str(text);
    }

    /// Read up to `max_chars` of new output, advance offset, compact if needed.
    fn read_chunk(&mut self, max_chars: usize) -> String {
        let end = (self.read_offset + max_chars).min(self.data.len());
        let chunk = self.data[self.read_offset..end].to_owned();
        self.read_offset += chunk.len();
        // Compact: if the consumed prefix is large, drop it
        if self.read_offset > COMPACT_THRESHOLD {
            self.data = self.data[self.read_offset..].to_owned();
            self.read_offset = 0;
        }
        chunk
    }
}

pub struct CmdSession {
    pub session_id: String,
    pub cmd: String,
    pub cwd: String,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    writer: Mutex<Box<dyn Write + Send>>,
    buffer: Arc<Mutex<CmdBuffer>>,
}

// SAFETY: CmdSession is Send + Sync because all mutable state is behind Mutex.
unsafe impl Send for CmdSession {}
unsafe impl Sync for CmdSession {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedCmdSession {
    session_id: String,
    cmd: String,
    cwd: String,
    running: bool,
    exit_code: Option<i64>,
    last_output: String,
}

#[derive(Debug, Clone)]
pub struct CommandSessionSnapshot {
    pub session_id: String,
    pub cmd: String,
    pub cwd: String,
    pub running: bool,
    pub exit_code: Option<i64>,
    pub last_output: String,
}

// ---------------------------------------------------------------------------
// Command session operations
// ---------------------------------------------------------------------------

pub fn start_command_session(
    sessions: &Arc<Mutex<HashMap<String, Arc<CmdSession>>>>,
    cwd: &Path,
    cmd: &str,
) -> Result<Value> {
    if is_blocked_command(cmd) {
        bail!("Blocked potentially destructive command");
    }

    let session = spawn_command_session(cwd, cmd)?;
    let session_id = session.session_id.clone();
    let cwd_str = session.cwd.clone();
    let running = session.child.lock().unwrap().process_id().is_some();

    sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), session.clone());
    persist_snapshot(&session, true, None)?;

    Ok(json!({
        "session_id": session_id,
        "cmd": cmd,
        "cwd": cwd_str,
        "running": running,
        "exit_code": null,
    }))
}

pub fn run_command(
    sessions: &Arc<Mutex<HashMap<String, Arc<CmdSession>>>>,
    cwd: &Path,
    cmd: &str,
) -> Result<String> {
    if is_blocked_command(cmd) {
        bail!("Blocked potentially destructive command");
    }

    let session = spawn_command_session(cwd, cmd)?;
    let session_id = session.session_id.clone();
    sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), session.clone());

    let deadline = std::time::Instant::now() + run_command_timeout();
    loop {
        let exit_code = session
            .child
            .lock()
            .unwrap()
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.exit_code() as i64);

        if let Some(exit_code) = exit_code {
            let output = {
                let buffer = session.buffer.lock().unwrap();
                truncate_output(&buffer.data, MAX_COMMAND_OUTPUT_CHARS)
            };
            persist_snapshot(&session, false, Some(exit_code))?;
            sessions.lock().unwrap().remove(&session_id);
            return Ok(format!(
                "$ {cmd}\n\nexit_code: {exit_code}\n\nstdout:\n{output}\n\nstderr:\n"
            ));
        }

        if std::time::Instant::now() >= deadline {
            let output = {
                let buffer = session.buffer.lock().unwrap();
                truncate_output(&buffer.data, MAX_COMMAND_OUTPUT_CHARS)
            };
            persist_snapshot(&session, true, None)?;
            return Ok(format!(
                "$ {cmd}\n\nsession_id: {session_id}\n\nrunning: true\n\nstdout:\n{output}\n\nstderr:\n\n[command is still running in session `{session_id}`; use read_command_session_tool to follow it or terminate_command_session_tool to stop it]"
            ));
        }

        std::thread::sleep(RUN_COMMAND_POLL_INTERVAL);
    }
}

pub fn read_command_session(
    sessions: &Arc<Mutex<HashMap<String, Arc<CmdSession>>>>,
    session_id: &str,
    max_chars: usize,
) -> Result<Value> {
    let session = {
        let guard = sessions.lock().unwrap();
        guard.get(session_id).cloned()
    };

    let Some(session) = session else {
        let persisted = load_session_metadata(session_id)?
            .ok_or_else(|| anyhow::anyhow!("Unknown command session"))?;
        return Ok(json!({
            "session_id": persisted.session_id,
            "cmd": persisted.cmd,
            "running": persisted.running,
            "exit_code": persisted.exit_code,
            "output": persisted.last_output,
        }));
    };

    let chunk = session.buffer.lock().unwrap().read_chunk(max_chars);
    let exit_code = session
        .child
        .lock()
        .unwrap()
        .try_wait()
        .ok()
        .flatten()
        .map(|status| status.exit_code() as i64);

    let running = exit_code.is_none();
    persist_snapshot(&session, running, exit_code)?;

    if !running {
        sessions.lock().unwrap().remove(session_id);
    }

    Ok(json!({
        "session_id": session_id,
        "cmd": session.cmd,
        "running": running,
        "exit_code": exit_code,
        "output": chunk,
    }))
}

pub fn list_command_sessions(
    sessions: &Arc<Mutex<HashMap<String, Arc<CmdSession>>>>,
) -> Result<Value> {
    let snapshot: Vec<Arc<CmdSession>> = sessions.lock().unwrap().values().cloned().collect();
    let mut items = Vec::with_capacity(snapshot.len());
    let mut seen = std::collections::HashSet::new();

    for session in snapshot {
        let exit_code = session
            .child
            .lock()
            .unwrap()
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.exit_code() as i64);
        let running = exit_code.is_none();
        let last_output = {
            let buffer = session.buffer.lock().unwrap();
            session_output_tail(&buffer.data)
        };

        persist_snapshot(&session, running, exit_code)?;

        items.push(json!({
            "session_id": session.session_id,
            "cmd": session.cmd,
            "cwd": session.cwd,
            "running": running,
            "exit_code": exit_code,
            "last_output": last_output,
        }));
        seen.insert(session.session_id.clone());
    }

    for persisted in list_persisted_sessions()? {
        if seen.contains(&persisted.session_id) {
            continue;
        }
        items.push(json!({
            "session_id": persisted.session_id,
            "cmd": persisted.cmd,
            "cwd": persisted.cwd,
            "running": persisted.running,
            "exit_code": persisted.exit_code,
            "last_output": persisted.last_output,
        }));
    }

    Ok(Value::Array(items))
}

pub fn load_persisted_command_sessions_for_cwd(cwd: &Path) -> Result<Vec<CommandSessionSnapshot>> {
    let cwd = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string();
    let sessions = list_persisted_sessions()?
        .into_iter()
        .filter(|session| session.cwd == cwd)
        .map(|session| CommandSessionSnapshot {
            session_id: session.session_id,
            cmd: session.cmd,
            cwd: session.cwd,
            running: session.running,
            exit_code: session.exit_code,
            last_output: session.last_output,
        })
        .collect();
    Ok(sessions)
}

pub fn write_command_session(
    sessions: &Arc<Mutex<HashMap<String, Arc<CmdSession>>>>,
    session_id: &str,
    chars: &str,
) -> Result<Value> {
    let session = {
        let guard = sessions.lock().unwrap();
        guard
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown command session"))?
    };

    // Check if still running
    let still_running = session
        .child
        .lock()
        .unwrap()
        .try_wait()
        .ok()
        .flatten()
        .is_none();
    if !still_running {
        bail!("Command session is not running");
    }

    session
        .writer
        .lock()
        .unwrap()
        .write_all(chars.as_bytes())
        .context("failed to write to PTY")?;

    Ok(json!({
        "session_id": session_id,
        "cmd": session.cmd,
        "running": true,
        "written_chars": chars.len(),
    }))
}

pub fn terminate_command_session(
    sessions: &Arc<Mutex<HashMap<String, Arc<CmdSession>>>>,
    session_id: &str,
    kill: bool,
) -> Result<Value> {
    let session = {
        let mut guard = sessions.lock().unwrap();
        guard.remove(session_id)
    };

    let Some(session) = session else {
        let mut persisted = load_session_metadata(session_id)?
            .ok_or_else(|| anyhow::anyhow!("Unknown command session"))?;
        persisted.running = false;
        persist_session_metadata(&persisted)?;
        return Ok(json!({
            "session_id": persisted.session_id,
            "cmd": persisted.cmd,
            "running": false,
            "exit_code": persisted.exit_code,
        }));
    };

    let mut child = session.child.lock().unwrap();

    // Send signal
    if child.try_wait().ok().flatten().is_none() {
        if kill {
            child.kill().ok();
        } else {
            // Send SIGTERM; portable-pty's kill() sends SIGKILL, so we use process group
            #[cfg(unix)]
            {
                if let Some(pid) = child.process_id() {
                    unsafe {
                        libc::killpg(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }
            #[cfg(not(unix))]
            child.kill().ok();
        }
    }

    // Wait up to 5 s
    let exit_code = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                break status.exit_code() as i64;
            }
            if std::time::Instant::now() >= deadline {
                // Force kill if we timed out on SIGTERM
                child.kill().ok();
                break child.wait().map(|s| s.exit_code() as i64).unwrap_or(-1);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    persist_snapshot(&session, false, Some(exit_code))?;

    Ok(json!({
        "session_id": session_id,
        "cmd": session.cmd,
        "running": false,
        "exit_code": exit_code,
    }))
}

// ---------------------------------------------------------------------------
// Persistence helpers
// ---------------------------------------------------------------------------

fn session_output_tail(output: &str) -> String {
    if output.len() <= MAX_PERSISTED_OUTPUT_CHARS {
        return output.to_owned();
    }
    let start = floor_char_boundary(output, output.len() - MAX_PERSISTED_OUTPUT_CHARS);
    output[start..].to_owned()
}

fn run_command_timeout() -> std::time::Duration {
    #[cfg(test)]
    {
        if let Ok(value) = std::env::var("MLX_ACP_RUN_COMMAND_TIMEOUT_MS") {
            if let Ok(ms) = value.parse::<u64>() {
                return std::time::Duration::from_millis(ms);
            }
        }
    }

    std::time::Duration::from_secs(60)
}

fn spawn_command_session(cwd: &Path, cmd: &str) -> Result<Arc<CmdSession>> {
    let session_id = format!("cmdsess_{}", &Uuid::new_v4().simple().to_string()[..12]);
    let cwd_str = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open PTY")?;

    let mut builder = CommandBuilder::new("sh");
    builder.arg("-c");
    builder.arg(cmd);
    builder.cwd(cwd);
    for (key, val) in std::env::vars() {
        builder.env(key, val);
    }
    builder.env(
        "TERM",
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_owned()),
    );

    let child = pair
        .slave
        .spawn_command(builder)
        .context("failed to spawn command in PTY")?;
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("failed to take PTY writer")?;

    let buffer = Arc::new(Mutex::new(CmdBuffer::new()));
    let buffer_for_thread = buffer.clone();

    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    buffer_for_thread.lock().unwrap().push(&text);
                }
            }
        }
        buffer_for_thread.lock().unwrap().closed = true;
    });

    Ok(Arc::new(CmdSession {
        session_id,
        cmd: cmd.to_owned(),
        cwd: cwd_str,
        child: Mutex::new(child),
        writer: Mutex::new(writer),
        buffer,
    }))
}

fn persist_snapshot(session: &CmdSession, running: bool, exit_code: Option<i64>) -> Result<()> {
    let last_output = {
        let buffer = session.buffer.lock().unwrap();
        session_output_tail(&buffer.data)
    };
    persist_session_metadata(&PersistedCmdSession {
        session_id: session.session_id.clone(),
        cmd: session.cmd.clone(),
        cwd: session.cwd.clone(),
        running,
        exit_code,
        last_output,
    })
}

fn sessions_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(path) = TEST_SESSIONS_DIR
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return path;
        }
    }

    if let Ok(path) = std::env::var("MLX_ACP_SESSIONS_DIR") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".mlx-acp-agent")
        .join("sessions")
}

#[cfg(test)]
pub(crate) fn set_test_sessions_dir(path: &Path) {
    *TEST_SESSIONS_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
}

#[cfg(test)]
pub(crate) fn clear_test_sessions_dir() {
    *TEST_SESSIONS_DIR.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
pub(crate) fn with_test_sessions_dir<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = TEST_SESSIONS_DIR_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    set_test_sessions_dir(path);
    let result = f();
    clear_test_sessions_dir();
    result
}

fn session_metadata_path(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{session_id}.json"))
}

fn persist_session_metadata(session: &PersistedCmdSession) -> Result<()> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create sessions dir {}", dir.display()))?;
    let path = session_metadata_path(&session.session_id);
    let data = serde_json::to_vec_pretty(session).context("failed to encode session metadata")?;
    fs::write(&path, data)
        .with_context(|| format!("failed to write session metadata {}", path.display()))?;
    Ok(())
}

fn load_session_metadata(session_id: &str) -> Result<Option<PersistedCmdSession>> {
    let path = session_metadata_path(session_id);
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read(&path)
        .with_context(|| format!("failed to read session metadata {}", path.display()))?;
    let session = serde_json::from_slice(&data)
        .with_context(|| format!("failed to decode session metadata {}", path.display()))?;
    Ok(Some(session))
}

fn list_persisted_sessions() -> Result<Vec<PersistedCmdSession>> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("failed to read sessions dir {}", dir.display()))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(_) => continue,
        };
        let session = match serde_json::from_slice::<PersistedCmdSession>(&data) {
            Ok(session) => session,
            Err(_) => continue,
        };
        sessions.push(session);
    }
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(sessions)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub fn is_blocked_command(cmd: &str) -> bool {
    let normalized = cmd
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let blocked_fragments = [
        "sudo ",
        " shutdown",
        " reboot",
        " poweroff",
        "halt ",
        "mkfs",
        "fdisk",
        "diskutil erase",
        "git reset --hard",
        "git checkout --",
        "git clean -fd",
        "git clean -xdf",
        ":(){",
    ];
    blocked_fragments
        .iter()
        .any(|fragment| normalized.contains(fragment))
}

fn truncate_output(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    // Keep the tail so that summary lines (e.g. "test result: ok. 69 passed")
    // are always visible even when the output is very long.
    let omitted = text.len() - limit;
    let start = ceil_char_boundary(text, omitted);
    format!("[truncated {omitted} chars]\n{}", &text[start..])
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::Value;
    use tempfile::TempDir;

    use super::{
        is_blocked_command, list_command_sessions, read_command_session, run_command,
        start_command_session,
    };

    #[test]
    fn blocks_destructive_commands() {
        assert!(is_blocked_command("git reset --hard HEAD"));
        assert!(is_blocked_command("sudo rm -rf /"));
        assert!(!is_blocked_command("git status"));
    }

    #[test]
    fn formats_command_output_like_python() {
        let tempdir = TempDir::new().expect("tempdir");
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let output = run_command(&sessions, tempdir.path(), "printf 'hi'").expect("run command");
        assert_eq!(
            output,
            "$ printf 'hi'\n\nexit_code: 0\n\nstdout:\nhi\n\nstderr:\n"
        );
        assert!(sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn long_running_run_command_returns_session_id_and_leaves_session_active() {
        let tempdir = TempDir::new().expect("tempdir");
        super::with_test_sessions_dir(tempdir.path(), || {
            unsafe {
                std::env::set_var("MLX_ACP_RUN_COMMAND_TIMEOUT_MS", "150");
            }
            let sessions = Arc::new(Mutex::new(HashMap::new()));
            let output = run_command(&sessions, tempdir.path(), "printf 'ready\\n'; sleep 1")
                .expect("run command");

            assert!(output.contains("session_id: cmdsess_"));
            assert!(output.contains("running: true"));
            assert_eq!(sessions.lock().unwrap().len(), 1);

            let listed = list_command_sessions(&sessions).expect("list");
            let items = listed.as_array().expect("array");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["running"], Value::Bool(true));
            assert!(items[0]["last_output"].as_str().unwrap().contains("ready"));
            unsafe {
                std::env::remove_var("MLX_ACP_RUN_COMMAND_TIMEOUT_MS");
            }
        });
    }

    #[test]
    fn persisted_sessions_are_listed_after_restart() {
        let tempdir = TempDir::new().expect("tempdir");
        super::with_test_sessions_dir(tempdir.path(), || {
            let sessions = Arc::new(Mutex::new(HashMap::new()));
            let started = start_command_session(&sessions, tempdir.path(), "printf 'ready\\n'")
                .expect("start");
            let session_id = started["session_id"].as_str().unwrap().to_owned();

            std::thread::sleep(Duration::from_millis(150));
            let _ = read_command_session(&sessions, &session_id, 4000).expect("read");

            let empty_sessions = Arc::new(Mutex::new(HashMap::new()));
            let listed = list_command_sessions(&empty_sessions).expect("list");
            let items = listed.as_array().expect("array");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["session_id"], Value::String(session_id));
            assert_eq!(items[0]["running"], Value::Bool(false));
            assert!(items[0]["last_output"].as_str().unwrap().contains("ready"));
        });
    }

    #[test]
    fn read_command_session_falls_back_to_persisted_metadata() {
        let tempdir = TempDir::new().expect("tempdir");
        super::with_test_sessions_dir(tempdir.path(), || {
            let sessions = Arc::new(Mutex::new(HashMap::new()));
            let started = start_command_session(&sessions, tempdir.path(), "printf 'ready\\n'")
                .expect("start");
            let session_id = started["session_id"].as_str().unwrap().to_owned();

            std::thread::sleep(Duration::from_millis(150));
            let first_read = read_command_session(&sessions, &session_id, 4000).expect("read");
            assert!(first_read["output"].as_str().unwrap().contains("ready"));

            let empty_sessions = Arc::new(Mutex::new(HashMap::new()));
            let restored = read_command_session(&empty_sessions, &session_id, 4000).expect("read");
            assert_eq!(restored["session_id"], Value::String(session_id));
            assert_eq!(restored["running"], Value::Bool(false));
            assert!(restored["output"].as_str().unwrap().contains("ready"));
        });
    }
}
