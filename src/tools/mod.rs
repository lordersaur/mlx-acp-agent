use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::agent_loop::ModelClient;
use crate::mlx_client::ChatMessage;

pub mod command;
pub mod fs;
pub mod web;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolProgressEvent {
    Reasoning { summary: String },
    FileModified { path: String, status: String },
    TerminalOutput { session_id: String, output: String },
}

pub trait ToolProgressSink: Send + Sync {
    fn emit(&self, event: ToolProgressEvent);
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BuiltinToolRegistry {
    workspace_cwd: PathBuf,
    model: Option<Arc<dyn ModelClient>>,
    progress: Option<Arc<dyn ToolProgressSink>>,
    /// Active command sessions shared across clones.
    pub cmd_sessions: Arc<Mutex<HashMap<String, Arc<command::CmdSession>>>>,
}

impl BuiltinToolRegistry {
    pub fn new(workspace_cwd: impl Into<PathBuf>) -> Result<Self> {
        let workspace_cwd = workspace_cwd.into();
        let workspace_cwd = workspace_cwd.canonicalize().unwrap_or(workspace_cwd);
        if !workspace_cwd.exists() {
            bail!("workspace does not exist: {}", workspace_cwd.display());
        }
        Ok(Self {
            workspace_cwd,
            model: None,
            progress: None,
            cmd_sessions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn workspace_cwd(&self) -> &std::path::Path {
        &self.workspace_cwd
    }

    pub fn new_with_model(
        workspace_cwd: impl Into<PathBuf>,
        model: Arc<dyn ModelClient>,
    ) -> Result<Self> {
        let mut registry = Self::new(workspace_cwd)?;
        registry.model = Some(model);
        Ok(registry)
    }

    pub fn with_progress_sink(mut self, progress: Arc<dyn ToolProgressSink>) -> Self {
        self.progress = Some(progress);
        self
    }

    fn require_model(&self) -> Result<&dyn ModelClient> {
        self.model
            .as_ref()
            .map(|m| m.as_ref())
            .ok_or_else(|| anyhow::anyhow!("this tool requires a model but none is configured"))
    }

    fn emit_progress(&self, event: ToolProgressEvent) {
        if let Some(progress) = &self.progress {
            progress.emit(event);
        }
    }

    // -----------------------------------------------------------------------
    // Schemas
    // -----------------------------------------------------------------------

    pub fn tool_schemas(&self) -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "read_file_tool",
                    "description": "Read a file from the workspace. Returns up to `limit` lines starting at line `start_line` (1-based). If the file is larger a continuation notice tells you the next start_line to use. search_code_tool returns 1-based line numbers — pass them directly as start_line.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Path to the file, relative to the workspace root."},
                            "start_line": {"type": "integer", "description": "1-based line number to start reading from (default 1)."},
                            "limit": {"type": "integer", "description": "Max number of lines to return (default 200)."}
                        },
                        "required": ["path"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "list_dir_tool",
                    "description": "List files/directories in the workspace. Use this when you need to inspect structure or locate files.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Directory path to inspect. Use \".\" for the current workspace root."}
                        }
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "search_code_tool",
                    "description": "Search the current workspace for code, text, symbols, or file references.\n\nUse this to inspect medium or large repositories before reading files. Prefer this over guessing paths.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string", "description": "Text or regex-like ripgrep query to search for."},
                            "glob": {"type": "string", "description": "Optional glob filter such as *.rs or src/**/*.py."}
                        },
                        "required": ["query"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "web_search_tool",
                    "description": "Search the web and return titles, URLs, and snippets.\n\nUse this to find documentation, bug reports, references, or relevant pages before using web_fetch_tool.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string", "description": "Search query."},
                            "max_results": {"type": "integer", "description": "Maximum number of results to return (default 5)."}
                        },
                        "required": ["query"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "web_fetch_tool",
                    "description": "Fetch a URL and return readable text content.\n\nUse this to read documentation, API specs, GitHub pages, health endpoints, or any URL the user references.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "url": {"type": "string", "description": "URL to fetch."}
                        },
                        "required": ["url"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "run_command_tool",
                    "description": "Run a shell command inside the current workspace.\n\nUse this for git, rg, tests, builds, linting, Flutter/Dart/Node commands, and deploy scripts when needed.\nDo not use destructive commands unless the user explicitly asks for them.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "cmd": {"type": "string", "description": "Shell command to execute."}
                        },
                        "required": ["cmd"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "list_command_sessions_tool",
                    "description": "List active command sessions in the workspace.\n\nUse this to see running terminal sessions, inspect their commands, and decide whether to read or terminate one.",
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "start_command_session_tool",
                    "description": "Start a persistent shell command session in the workspace.\n\nUse this when the user wants to follow a running command, tail logs, watch output, or interact with a process over time.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "cmd": {"type": "string", "description": "Shell command to run in the session."}
                        },
                        "required": ["cmd"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "read_command_session_tool",
                    "description": "Read new output from a previously started command session.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "session_id": {"type": "string", "description": "Session ID returned by start_command_session_tool."},
                            "max_chars": {"type": "integer", "description": "Maximum characters to return (default 4000)."}
                        },
                        "required": ["session_id"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "write_command_session_tool",
                    "description": "Write stdin to a running command session.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "session_id": {"type": "string", "description": "Session ID."},
                            "chars": {"type": "string", "description": "Characters to write to stdin."}
                        },
                        "required": ["session_id", "chars"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "terminate_command_session_tool",
                    "description": "Terminate a running command session.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "session_id": {"type": "string", "description": "Session ID."},
                            "kill": {"type": "boolean", "description": "Use SIGKILL instead of SIGTERM."}
                        },
                        "required": ["session_id"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "patch_file_tool",
                    "description": "Apply a targeted text patch to an existing file.\n\nUse this when you know the exact snippet to replace and want a safer scoped edit than rewriting the entire file.\nRead or search the file first so the patch target is precise. Keep patches small: replace a single expression, helper, or adjacent block instead of an entire function whenever possible.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Path to the file to patch, relative to the workspace root."},
                            "old_text": {"type": "string", "description": "Exact existing text to replace. Keep this small and scoped; do not paste an entire function unless unavoidable."},
                            "new_text": {"type": "string", "description": "Replacement text. Keep this small and scoped; prefer adding a helper plus one call-site change over rewriting a whole function."},
                            "replace_all": {"type": "boolean", "description": "Whether to replace every occurrence instead of just one."}
                        },
                        "required": ["path", "old_text", "new_text"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "delete_path_tool",
                    "description": "Delete a file, symlink, or directory in the workspace.\n\nUse this when the user explicitly asks to delete or remove a path.\nFor non-empty directories, set recursive=true only when the request clearly requires it.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Path to delete, relative to the workspace root."},
                            "recursive": {"type": "boolean", "description": "Delete non-empty directories recursively when true."}
                        },
                        "required": ["path"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "edit_file_tool",
                    "description": "Rewrite an entire small existing file in the workspace.\n\nDo not use this for targeted edits, appends, refactors inside large files, or changes where exact old/new text can be identified. Prefer patch_file_tool for those cases.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Path to the file to edit, relative to the workspace root."},
                            "instruction": {"type": "string", "description": "What to change in the file."}
                        },
                        "required": ["path", "instruction"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "create_artifact_tool",
                    "description": "Create and write a new file in the workspace.\n\nUse this whenever the user asks to create, make, write, save, or generate a file, note, document, config, or code artifact.\nIf the user names an exact filename, pass that filename.\nDo not answer with the file contents directly when this tool should be used.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "instruction": {"type": "string", "description": "What the file should contain."},
                            "filename": {"type": "string", "description": "Optional filename. If omitted the model infers one."},
                            "kind": {"type": "string", "description": "Optional file kind hint: markdown, json, dart, python, yaml, text."}
                        },
                        "required": ["instruction"]
                    }
                }
            }),
        ]
    }

    // -----------------------------------------------------------------------
    // Invocation helpers
    // -----------------------------------------------------------------------

    fn invoke_read_file(&self, arguments: Map<String, Value>) -> Result<String> {
        let path = required_string(&arguments, "path")?;
        let start_line = arguments
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(1)
            .max(1);
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(200);
        let content = fs::read_file(&self.workspace_cwd, path)?;
        Ok(file_chunk_lines(&content, start_line, limit))
    }

    fn invoke_list_dir(&self, arguments: Map<String, Value>) -> Result<String> {
        let path = optional_string(&arguments, "path").unwrap_or(".");
        let entries = fs::list_dir(&self.workspace_cwd, path)?;
        Ok(entries.join("\n"))
    }

    fn invoke_search_code(&self, arguments: Map<String, Value>) -> Result<String> {
        let query = required_string(&arguments, "query")?;
        let glob = optional_string(&arguments, "glob");
        fs::search_code(&self.workspace_cwd, query, glob)
    }

    async fn invoke_web_search(&self, arguments: Map<String, Value>) -> Result<String> {
        let query = required_string(&arguments, "query")?;
        let max_results = arguments
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(5) as usize;
        web::search_web(query, max_results).await
    }

    async fn invoke_web_fetch(&self, arguments: Map<String, Value>) -> Result<String> {
        let url = required_string(&arguments, "url")?;
        web::fetch_url(url).await
    }

    fn invoke_run_command(&self, arguments: Map<String, Value>) -> Result<String> {
        let cmd = required_string(&arguments, "cmd")?;
        command::run_command(&self.cmd_sessions, &self.workspace_cwd, cmd).map_err(|error| {
            eprintln!("[run_command_tool] command failed: cmd={cmd:?}, error={error:?}");
            error
        })
    }

    fn invoke_start_command_session(&self, arguments: Map<String, Value>) -> Result<String> {
        let cmd = required_string(&arguments, "cmd")?;
        let result = command::start_command_session(&self.cmd_sessions, &self.workspace_cwd, cmd)?;
        Ok(result.to_string())
    }

    fn invoke_list_command_sessions(&self) -> Result<String> {
        let result = command::list_command_sessions(&self.cmd_sessions)?;
        Ok(result.to_string())
    }

    fn invoke_read_command_session(&self, arguments: Map<String, Value>) -> Result<String> {
        let session_id = required_string(&arguments, "session_id")?;
        let max_chars = arguments
            .get("max_chars")
            .and_then(Value::as_u64)
            .unwrap_or(4000) as usize;
        let result = command::read_command_session(&self.cmd_sessions, session_id, max_chars)?;
        if let Some(output) = result.get("output").and_then(Value::as_str) {
            if !output.trim().is_empty() {
                self.emit_progress(ToolProgressEvent::TerminalOutput {
                    session_id: session_id.to_owned(),
                    output: output.to_owned(),
                });
            }
        }
        Ok(result.to_string())
    }

    fn invoke_write_command_session(&self, arguments: Map<String, Value>) -> Result<String> {
        let session_id = required_string(&arguments, "session_id")?;
        let chars = required_string(&arguments, "chars")?;
        let result = command::write_command_session(&self.cmd_sessions, session_id, chars)?;
        Ok(result.to_string())
    }

    fn invoke_terminate_command_session(&self, arguments: Map<String, Value>) -> Result<String> {
        let session_id = required_string(&arguments, "session_id")?;
        let kill = optional_bool(&arguments, "kill").unwrap_or(false);
        let result = command::terminate_command_session(&self.cmd_sessions, session_id, kill)?;
        Ok(result.to_string())
    }

    fn invoke_patch_file(&self, arguments: Map<String, Value>) -> Result<String> {
        let path = required_string(&arguments, "path")?;
        let old_text = required_string(&arguments, "old_text")?;
        let new_text = required_string(&arguments, "new_text")?;
        let replace_all = optional_bool(&arguments, "replace_all").unwrap_or(false);
        let written = fs::apply_patch(&self.workspace_cwd, path, old_text, new_text, replace_all)?;
        self.emit_progress(ToolProgressEvent::FileModified {
            path: written.clone(),
            status: "patched".to_owned(),
        });
        Ok(json!({
            "status": "patched",
            "path": written,
            "replace_all": replace_all,
            "preview": patch_preview(old_text, new_text)
        })
        .to_string())
    }

    fn invoke_delete_path(&self, arguments: Map<String, Value>) -> Result<String> {
        let path = required_string(&arguments, "path")?;
        let recursive = optional_bool(&arguments, "recursive").unwrap_or(false);
        let deleted = fs::delete_path(&self.workspace_cwd, path, recursive)?;
        self.emit_progress(ToolProgressEvent::FileModified {
            path: deleted.clone(),
            status: "deleted".to_owned(),
        });
        Ok(json!({
            "status": "deleted",
            "path": deleted,
            "recursive": recursive
        })
        .to_string())
    }

    async fn invoke_edit_file(&self, arguments: Map<String, Value>) -> Result<String> {
        let path = required_string(&arguments, "path")?;
        let instruction = required_string(&arguments, "instruction")?;
        let model = self.require_model()?;

        let original = fs::read_file(&self.workspace_cwd, path)?;
        let original_len = original.len();
        let messages = [
            ChatMessage::system(concat!(
                "You are editing a file.\n",
                "Return only the full updated file contents.\n",
                "Preserve ALL unrelated content exactly as-is — do not summarise, omit, or truncate any part of the file.\n",
                "Do not explain changes.\n",
                "Do not use code fences."
            )),
            ChatMessage::user(format!(
                "Path: {path}\n\nInstruction:\n{instruction}\n\nCurrent file:\n{original}"
            )),
        ];
        let rewritten = sanitize_generated_file_content(
            &model
                .complete(&messages, &[], 2500, 0.0, None)
                .await?
                .content
                .unwrap_or_default(),
        );

        // Guard against truncated output: if the model returned less than 60% of
        // the original length, the rewrite is almost certainly incomplete.
        if original_len > 200 && rewritten.len() < original_len * 6 / 10 {
            anyhow::bail!(
                "edit_file_tool: model output ({} chars) is too short compared to the original \
                 file ({} chars) — refusing to write a likely-truncated result. \
                 Use patch_file_tool to make targeted edits to large files.",
                rewritten.len(),
                original_len
            );
        }

        let written = fs::write_file(&self.workspace_cwd, path, &rewritten)?;
        self.emit_progress(ToolProgressEvent::FileModified {
            path: written.clone(),
            status: "updated".to_owned(),
        });
        Ok(json!({
            "status": "updated",
            "path": written,
            "preview": file_preview(&rewritten)
        })
        .to_string())
    }

    async fn invoke_create_artifact(&self, arguments: Map<String, Value>) -> Result<String> {
        let instruction = required_string(&arguments, "instruction")?;
        let filename = optional_string(&arguments, "filename");
        let kind = optional_string(&arguments, "kind");
        let model = self.require_model()?;

        self.emit_progress(ToolProgressEvent::Reasoning {
            summary: "Planning a new artifact before generating file contents.".to_owned(),
        });

        let resolved_kind = if let Some(k) = kind {
            k.to_owned()
        } else {
            infer_kind(model, instruction).await?
        };
        let final_name = if let Some(f) = filename {
            f.to_owned()
        } else if let Some(f) = exact_filename_from_instruction(instruction) {
            f
        } else {
            infer_filename(model, instruction, &resolved_kind).await?
        };
        self.emit_progress(ToolProgressEvent::Reasoning {
            summary: format!("Generating `{final_name}` as {resolved_kind}."),
        });

        let content_messages = [
            ChatMessage::system(format!(
                "You generate file contents only.\nReturn only valid {resolved_kind} content.\nDo not add explanations.\nDo not use code fences."
            )),
            ChatMessage::user(instruction),
        ];
        let content = sanitize_generated_file_content(
            &model
                .complete(&content_messages, &[], 1400, 0.1, None)
                .await?
                .content
                .unwrap_or_default(),
        );
        let written = fs::write_file(&self.workspace_cwd, &final_name, &content)?;
        self.emit_progress(ToolProgressEvent::FileModified {
            path: written.clone(),
            status: "created".to_owned(),
        });
        Ok(json!({
            "status": "created",
            "filename": final_name,
            "path": written,
            "preview": file_preview(&content)
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// ToolExecutor impl
// ---------------------------------------------------------------------------

use crate::agent_loop::ToolExecutor;

#[async_trait]
impl ToolExecutor for BuiltinToolRegistry {
    fn tool_names(&self) -> Vec<String> {
        self.tool_schemas()
            .into_iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    async fn invoke(&self, name: &str, arguments: Map<String, Value>) -> Result<String> {
        match name {
            "read_file_tool" => self.invoke_read_file(arguments),
            "list_dir_tool" => self.invoke_list_dir(arguments),
            "search_code_tool" => self.invoke_search_code(arguments),
            "web_search_tool" => self.invoke_web_search(arguments).await,
            "web_fetch_tool" => self.invoke_web_fetch(arguments).await,
            "run_command_tool" => self.invoke_run_command(arguments),
            "list_command_sessions_tool" => self.invoke_list_command_sessions(),
            "start_command_session_tool" => self.invoke_start_command_session(arguments),
            "read_command_session_tool" => self.invoke_read_command_session(arguments),
            "write_command_session_tool" => self.invoke_write_command_session(arguments),
            "terminate_command_session_tool" => self.invoke_terminate_command_session(arguments),
            "patch_file_tool" => self.invoke_patch_file(arguments),
            "delete_path_tool" => self.invoke_delete_path(arguments),
            "edit_file_tool" => self.invoke_edit_file(arguments).await,
            "create_artifact_tool" => self.invoke_create_artifact(arguments).await,
            _ => bail!("Unknown tool: {name}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Filename helpers (create_artifact_tool)
// ---------------------------------------------------------------------------

fn slugify(text: &str) -> String {
    let text = text.to_lowercase();
    let text = text.trim().to_owned();
    let re1 = Regex::new(r"[^\w\s-]").unwrap();
    let text = re1.replace_all(&text, "").into_owned();
    let re2 = Regex::new(r"[\s_-]+").unwrap();
    let text = re2.replace_all(&text, "-").into_owned();
    let re3 = Regex::new(r"^-+|-+$").unwrap();
    let text = re3.replace_all(&text, "").into_owned();
    if text.is_empty() {
        "note".to_owned()
    } else {
        text
    }
}

fn infer_extension(kind: &str) -> &'static str {
    match kind.to_lowercase().as_str() {
        "markdown" | "md" => ".md",
        "json" => ".json",
        "dart" => ".dart",
        "python" => ".py",
        "yaml" | "yml" => ".yml",
        "text" | "txt" => ".txt",
        _ => ".txt",
    }
}

fn fallback_filename(instruction: &str, kind: &str) -> String {
    let capped = &instruction[..instruction.len().min(80)];
    format!("{}{}", slugify(capped), infer_extension(kind))
}

fn sanitize_generated_file_content(text: &str) -> String {
    let mut cleaned = text.replace("<|im_end|>", "").replace("<|im_start|>", "");

    for (open, close) in [("<thinking>", "</thinking>"), ("<think>", "</think>")] {
        while let Some(start) = cleaned.find(open) {
            if let Some(end_rel) = cleaned[start + open.len()..].find(close) {
                let end = start + open.len() + end_rel + close.len();
                cleaned.replace_range(start..end, "");
            } else {
                cleaned.truncate(start);
                break;
            }
        }
    }

    let balanced_channel_re = Regex::new(r"(?s)<\|channel>[^\n]*\n.*?<channel\|>").unwrap();
    cleaned = balanced_channel_re.replace_all(&cleaned, "").into_owned();

    let drop_channel_lines = Regex::new(r"(?m)^\s*(?:<\|channel>|<channel\|>).*$").unwrap();
    cleaned = drop_channel_lines.replace_all(&cleaned, "").into_owned();

    while let Some(start) = cleaned.find("<|channel>") {
        if let Some(end_rel) = cleaned[start..].find("<channel|>") {
            let end = start + end_rel + "<channel|>".len();
            cleaned.replace_range(start..end, "");
        } else {
            cleaned.truncate(start);
            break;
        }
    }

    cleaned = cleaned.replace("<channel|>", "").replace("<|channel>", "");

    let has_trailing_newline = cleaned.ends_with('\n');
    let cleaned = cleaned.trim().to_owned();
    if has_trailing_newline && !cleaned.is_empty() {
        format!("{cleaned}\n")
    } else {
        cleaned
    }
}

async fn infer_kind(model: &dyn ModelClient, instruction: &str) -> Result<String> {
    let messages = [
        ChatMessage::system(
            "Classify the best file kind for the request. Return only one of: markdown, json, dart, python, yaml, text",
        ),
        ChatMessage::user(instruction),
    ];
    let result = model.complete(&messages, &[], 20, 0.0, None).await?;
    let kind = result.content.unwrap_or_default().trim().to_lowercase();
    if matches!(
        kind.as_str(),
        "markdown" | "json" | "dart" | "python" | "yaml" | "text"
    ) {
        Ok(kind)
    } else {
        Ok("markdown".to_owned())
    }
}

async fn infer_filename(model: &dyn ModelClient, instruction: &str, kind: &str) -> Result<String> {
    let ext = infer_extension(kind);
    let messages = [
        ChatMessage::system(format!(
            "Generate a short descriptive filename.\n\
             Rules:\n- return only the filename\n- lowercase\n- kebab-case\n\
             - must end with {ext}\n- no directories\n- no backticks\n- no explanations\n"
        )),
        ChatMessage::user(instruction),
    ];
    let result = model.complete(&messages, &[], 40, 0.0, None).await?;

    let candidate = result
        .content
        .unwrap_or_default()
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('`')
        .to_owned();
    let candidate = candidate.replace('/', "-").replace('\\', "-");
    let re_ws = Regex::new(r"\s+").unwrap();
    let candidate = re_ws
        .replace_all(&candidate.to_lowercase(), "-")
        .into_owned();
    let re_invalid = Regex::new(r"[^a-z0-9._-]").unwrap();
    let candidate = re_invalid.replace_all(&candidate, "").into_owned();

    if candidate.is_empty() {
        return Ok(fallback_filename(instruction, kind));
    }
    if candidate.ends_with(ext) {
        Ok(candidate)
    } else if candidate.contains('.') {
        let base = candidate
            .rsplit_once('.')
            .map(|(b, _)| b)
            .unwrap_or(&candidate);
        Ok(format!("{base}{ext}"))
    } else {
        Ok(format!("{candidate}{ext}"))
    }
}

fn exact_filename_from_instruction(instruction: &str) -> Option<String> {
    let re = Regex::new(
        r"(?i)\b([A-Za-z0-9][A-Za-z0-9._-]*\.(?:md|txt|json|toml|ya?ml|rs|py|js|ts|tsx|jsx|dart|html|css))\b",
    )
    .unwrap();
    let filename = re.captures(instruction)?.get(1)?.as_str();
    Some(filename.trim_matches('`').to_owned())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn file_chunk_lines(content: &str, start_line: usize, limit: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if start_line > total {
        return format!("[start_line {start_line} is past end of file ({total} lines)]");
    }
    let from = start_line - 1; // convert to 0-based
    let to = (from + limit).min(total);
    let chunk = lines[from..to]
        .iter()
        .enumerate()
        .map(|(i, line)| format_file_line(from + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");
    if to >= total {
        if start_line == 1 {
            chunk
        } else {
            format!("{chunk}\n[End of file. Showed lines {start_line}–{to} of {total}.]")
        }
    } else {
        format!(
            "{chunk}\n[Showing lines {start_line}–{to} of {total}. Call read_file_tool with start_line={} to continue.]",
            to + 1
        )
    }
}

fn format_file_line(line_number: usize, line: &str) -> String {
    format!("{line_number}: {line}")
}

fn file_preview(content: &str) -> String {
    file_chunk_lines(content, 1, 80)
}

fn patch_preview(old_text: &str, new_text: &str) -> String {
    format!(
        "--- old\n{}\n+++ new\n{}",
        preview_text(old_text),
        preview_text(new_text)
    )
}

fn preview_text(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 6000;
    if text.len() <= MAX_PREVIEW_CHARS {
        return text.to_owned();
    }
    let mut end = 0;
    for (idx, _) in text.char_indices() {
        if idx > MAX_PREVIEW_CHARS {
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

fn required_string<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required string argument `{key}`"))
}

fn optional_string<'a>(arguments: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

fn optional_bool(arguments: &Map<String, Value>, key: &str) -> Option<bool> {
    arguments.get(key).and_then(Value::as_bool)
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
    use std::collections::VecDeque;
    use std::fs as stdfs;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::Mutex as AsyncMutex;

    use super::{
        BuiltinToolRegistry, fallback_filename, infer_extension, sanitize_generated_file_content,
        slugify,
    };
    use crate::agent_loop::{AgentLoopOptions, ConversationMessage, ToolExecutor, run_agent_loop};
    use crate::mlx_client::ChatMessage;

    struct MockModel {
        responses: AsyncMutex<VecDeque<crate::mlx_client::CompletionResult>>,
    }

    impl MockModel {
        /// Simple text-only responses (for inner model calls inside tools).
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: AsyncMutex::new(
                    responses
                        .into_iter()
                        .map(|s| crate::mlx_client::CompletionResult {
                            content: Some(s.to_owned()),
                            tool_calls: Vec::new(),
                        })
                        .collect(),
                ),
            }
        }

        /// Structured CompletionResult responses (for loop model in integration tests).
        fn with_results(responses: Vec<crate::mlx_client::CompletionResult>) -> Self {
            Self {
                responses: AsyncMutex::new(responses.into_iter().collect()),
            }
        }
    }

    /// Helper: build a tool-call CompletionResult for use in integration tests.
    fn tool_call_result(
        id: &str,
        name: &str,
        args: serde_json::Value,
    ) -> crate::mlx_client::CompletionResult {
        crate::mlx_client::CompletionResult {
            content: None,
            tool_calls: vec![crate::mlx_client::ApiToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: args.as_object().cloned().unwrap_or_default(),
            }],
        }
    }

    fn text_result(s: &str) -> crate::mlx_client::CompletionResult {
        crate::mlx_client::CompletionResult {
            content: Some(s.to_owned()),
            tool_calls: Vec::new(),
        }
    }

    #[async_trait]
    impl crate::agent_loop::ModelClient for MockModel {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
            _max_tokens: u32,
            _temperature: f32,
            _think_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        ) -> Result<crate::mlx_client::CompletionResult> {
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no mock responses left"))
        }
    }

    // -----------------------------------------------------------------------
    // Schema / registry smoke tests
    // -----------------------------------------------------------------------

    #[test]
    fn tool_schemas_expose_expected_names() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        let names = registry.tool_names();
        assert_eq!(
            names,
            vec![
                "read_file_tool",
                "list_dir_tool",
                "search_code_tool",
                "web_search_tool",
                "web_fetch_tool",
                "run_command_tool",
                "list_command_sessions_tool",
                "start_command_session_tool",
                "read_command_session_tool",
                "write_command_session_tool",
                "terminate_command_session_tool",
                "patch_file_tool",
                "delete_path_tool",
                "edit_file_tool",
                "create_artifact_tool",
            ]
        );
    }

    // -----------------------------------------------------------------------
    // Pure helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn slugify_produces_kebab_case() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  foo  bar  "), "foo-bar");
        assert_eq!(slugify("my_file_name"), "my-file-name");
        assert_eq!(slugify("!!!"), "note");
        assert_eq!(slugify(""), "note");
    }

    #[test]
    fn infer_extension_maps_known_kinds() {
        assert_eq!(infer_extension("markdown"), ".md");
        assert_eq!(infer_extension("json"), ".json");
        assert_eq!(infer_extension("dart"), ".dart");
        assert_eq!(infer_extension("python"), ".py");
        assert_eq!(infer_extension("yaml"), ".yml");
        assert_eq!(infer_extension("text"), ".txt");
        assert_eq!(infer_extension("unknown_kind"), ".txt");
    }

    #[test]
    fn fallback_filename_slugifies_instruction() {
        assert_eq!(
            fallback_filename("Write a project README", "markdown"),
            "write-a-project-readme.md"
        );
    }

    #[test]
    fn sanitize_generated_file_content_strips_meta_and_channel_leaks() {
        let raw = r#"
<thinking>
plan first
</thinking>
<|channel>thought
This should not be written.
<channel|>
# Title

Body text.
"#;

        assert_eq!(
            sanitize_generated_file_content(raw),
            "# Title\n\nBody text.\n"
        );
    }

    // -----------------------------------------------------------------------
    // command session tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_command_sessions_tool_returns_empty_array_when_no_sessions_exist() {
        let tempdir = TempDir::new().expect("tempdir");
        crate::tools::command::with_test_sessions_dir(tempdir.path(), || {
            let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
            let result = futures::executor::block_on(registry.invoke(
                "list_command_sessions_tool",
                json!({}).as_object().cloned().unwrap(),
            ))
            .expect("invoke");
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(parsed, json!([]));
        });
    }

    #[test]
    fn list_command_sessions_tool_reports_running_session() {
        let tempdir = TempDir::new().expect("tempdir");
        crate::tools::command::with_test_sessions_dir(tempdir.path(), || {
            let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
            futures::executor::block_on(
                registry.invoke(
                    "start_command_session_tool",
                    json!({"cmd": "printf 'ready\\n'; sleep 2"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
            )
            .expect("start session");

            std::thread::sleep(std::time::Duration::from_millis(100));

            let result = futures::executor::block_on(registry.invoke(
                "list_command_sessions_tool",
                json!({}).as_object().cloned().unwrap(),
            ))
            .expect("invoke");
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
            let items = parsed.as_array().expect("array");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["running"], true);
            assert_eq!(items[0]["cmd"], "printf 'ready\\n'; sleep 2");
        });
    }

    // -----------------------------------------------------------------------
    // edit_file_tool tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn edit_file_tool_rewrites_existing_file() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("notes.txt"), "original content").expect("write");
        let model = Arc::new(MockModel::new(vec!["updated content"]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        let result = registry
            .invoke(
                "edit_file_tool",
                json!({"path": "notes.txt", "instruction": "Make it better."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "updated");
        assert_eq!(
            stdfs::read_to_string(tempdir.path().join("notes.txt")).unwrap(),
            "updated content"
        );
    }

    #[tokio::test]
    async fn edit_file_tool_sanitizes_meta_before_writing() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("notes.txt"), "original content\n").expect("write");
        let model = Arc::new(MockModel::new(vec![
            "<|channel>thought\nHidden plan\n<channel|>\nupdated content\n",
        ]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        registry
            .invoke(
                "edit_file_tool",
                json!({"path": "notes.txt", "instruction": "Replace the contents."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        assert_eq!(
            stdfs::read_to_string(tempdir.path().join("notes.txt")).unwrap(),
            "updated content\n"
        );
    }

    #[tokio::test]
    async fn edit_file_tool_fails_on_missing_file() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec![]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");
        let err = registry
            .invoke(
                "edit_file_tool",
                json!({"path": "nonexistent.txt", "instruction": "Edit it."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect_err("should fail");
        assert!(
            err.to_string().contains("nonexistent.txt")
                || err.to_string().contains("failed to read"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn edit_file_tool_fails_without_model() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("f.txt"), "hello").unwrap();
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        let err = registry
            .invoke(
                "edit_file_tool",
                json!({"path": "f.txt", "instruction": "Edit."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("model"));
    }

    // -----------------------------------------------------------------------
    // create_artifact_tool tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_artifact_tool_creates_file_with_explicit_filename_and_kind() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec!["# Hello\nThis is the doc."]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        let result = registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "Write a short markdown doc", "filename": "doc.md", "kind": "markdown"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "created");
        assert_eq!(parsed["filename"], "doc.md");
        assert_eq!(
            stdfs::read_to_string(tempdir.path().join("doc.md")).unwrap(),
            "# Hello\nThis is the doc."
        );
    }

    #[tokio::test]
    async fn create_artifact_tool_sanitizes_meta_before_writing() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec![
            "markdown",
            "doc.md",
            "<thinking>draft</thinking>\n<|channel>thought\ninternal\n<channel|>\n# Hello\nThis is the doc.\n",
        ]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "Write a short markdown doc"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        assert_eq!(
            stdfs::read_to_string(tempdir.path().join("doc.md")).unwrap(),
            "# Hello\nThis is the doc.\n"
        );
    }

    #[tokio::test]
    async fn create_artifact_tool_infers_kind_and_filename() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec![
            "markdown",
            "my-note.md",
            "# My Note\nContent here.",
        ]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        let result = registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "Write a note about the project"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "created");
        assert_eq!(parsed["filename"], "my-note.md");
    }

    #[tokio::test]
    async fn create_artifact_tool_uses_fallback_when_model_returns_garbage_filename() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec![
            "text",
            "!!!invalid!!!",
            "plain content",
        ]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        let result = registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "A plain text note"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "created");
        assert_eq!(parsed["filename"], "invalid.txt");
    }

    #[tokio::test]
    async fn create_artifact_tool_fails_without_model() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        let err = registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "Write something"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("model"));
    }

    #[tokio::test]
    async fn create_artifact_tool_uses_exact_filename_from_instruction() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec![
            "markdown",
            "# Contributing\nRun `cargo test` before submitting.\n",
        ]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        let result = registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "Add a CONTRIBUTING.md file at the repo root."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "created");
        assert_eq!(parsed["filename"], "CONTRIBUTING.md");
        assert!(tempdir.path().join("CONTRIBUTING.md").exists());
    }

    // -----------------------------------------------------------------------
    // Existing integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn real_registry_can_complete_simple_inspect_flow() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("README.md"), "hello rust migration")
            .expect("write readme");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        let schemas = registry.tool_schemas();
        let model = MockModel::with_results(vec![
            tool_call_result("c1", "read_file_tool", json!({"path": "README.md"})),
            text_result("The README says hello rust migration."),
        ]);
        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new(
                "user",
                "Read README.md and summarize it.",
            )],
            &registry,
            &schemas,
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");
        assert_eq!(result.answer, "The README says hello rust migration.");
        assert_eq!(result.tool_results[0].name, "read_file_tool");
    }

    #[tokio::test]
    async fn real_registry_can_patch_and_delete_files() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("notes.txt"), "alpha\n").expect("write notes");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        let schemas = registry.tool_schemas();
        let model = MockModel::with_results(vec![
            tool_call_result(
                "c1",
                "patch_file_tool",
                json!({"path": "notes.txt", "old_text": "alpha", "new_text": "beta"}),
            ),
            tool_call_result("c2", "delete_path_tool", json!({"path": "notes.txt"})),
            text_result("Patched and deleted notes.txt."),
        ]);
        let result = run_agent_loop(
            &model,
            &[ConversationMessage::new(
                "user",
                "Patch notes.txt, then delete it.",
            )],
            &registry,
            &schemas,
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");
        assert_eq!(result.answer, "Patched and deleted notes.txt.");
        assert!(!tempdir.path().join("notes.txt").exists());
    }

    #[tokio::test]
    async fn real_registry_can_edit_file_through_agent_loop() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("notes.txt"), "original content\n").expect("write notes");
        let model = Arc::new(MockModel::new(vec!["updated content\n"]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");
        let schemas = registry.tool_schemas();
        let loop_model = MockModel::with_results(vec![
            tool_call_result(
                "c1",
                "edit_file_tool",
                json!({"path": "notes.txt", "instruction": "Replace the contents with updated content."}),
            ),
            text_result("Updated notes.txt."),
        ]);

        let result = run_agent_loop(
            &loop_model,
            &[ConversationMessage::new(
                "user",
                "Edit notes.txt and replace its contents.",
            )],
            &registry,
            &schemas,
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Updated notes.txt.");
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].name, "edit_file_tool");
        assert_eq!(
            stdfs::read_to_string(tempdir.path().join("notes.txt")).unwrap(),
            "updated content\n"
        );
    }

    #[tokio::test]
    async fn real_registry_can_create_artifact_through_agent_loop() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec![
            "markdown",
            "summary.md",
            "# Summary\nCreated by the tool.\n",
        ]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");
        let schemas = registry.tool_schemas();
        let loop_model = MockModel::with_results(vec![
            tool_call_result(
                "c1",
                "create_artifact_tool",
                json!({"instruction": "Write a short markdown summary file."}),
            ),
            text_result("Created summary.md."),
        ]);

        let result = run_agent_loop(
            &loop_model,
            &[ConversationMessage::new(
                "user",
                "Create a short markdown summary file.",
            )],
            &registry,
            &schemas,
            None,
            AgentLoopOptions::default(),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(result.answer, "Created summary.md.");
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].name, "create_artifact_tool");
        let parsed: serde_json::Value =
            serde_json::from_str(&result.tool_results[0].result).unwrap();
        assert_eq!(parsed["status"], "created");
        assert_eq!(parsed["filename"], "summary.md");
        assert_eq!(
            stdfs::read_to_string(tempdir.path().join("summary.md")).unwrap(),
            "# Summary\nCreated by the tool.\n"
        );
    }

    #[test]
    fn read_file_tool_paginates_large_content() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        // 450 lines — more than the default 200-line limit.
        let large = (1..=450)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        stdfs::write(tempdir.path().join("large.txt"), &large).expect("write large file");

        // First read: default start_line=1, limit=200.
        let page1 = futures::executor::block_on(registry.invoke(
            "read_file_tool",
            json!({"path": "large.txt"}).as_object().cloned().unwrap(),
        ))
        .expect("invoke page 1");
        assert!(
            page1.contains("start_line=201 to continue"),
            "page 1 should show continuation: {page1}"
        );

        // Second read: start_line=201 gets lines 201-400.
        let page2 = futures::executor::block_on(
            registry.invoke(
                "read_file_tool",
                json!({"path": "large.txt", "start_line": 201})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .expect("invoke page 2");
        assert!(
            page2.contains("start_line=401 to continue"),
            "page 2 should show continuation: {page2}"
        );

        // Third read: start_line=401 gets the rest.
        let page3 = futures::executor::block_on(
            registry.invoke(
                "read_file_tool",
                json!({"path": "large.txt", "start_line": 401})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .expect("invoke page 3");
        assert!(
            page3.contains("End of file"),
            "page 3 should reach end: {page3}"
        );

        // Small file: returned whole, no continuation notice.
        stdfs::write(tempdir.path().join("small.txt"), "hello").expect("write small file");
        let small = futures::executor::block_on(registry.invoke(
            "read_file_tool",
            json!({"path": "small.txt"}).as_object().cloned().unwrap(),
        ))
        .expect("invoke small");
        assert_eq!(small, "1: hello");
    }

    #[test]
    fn read_file_tool_reports_start_line_past_end() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        stdfs::write(tempdir.path().join("short.txt"), "one\ntwo\nthree\n")
            .expect("write short file");

        let output = futures::executor::block_on(
            registry.invoke(
                "read_file_tool",
                json!({"path": "short.txt", "start_line": 99})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .expect("invoke");

        assert_eq!(output, "[start_line 99 is past end of file (3 lines)]");
    }
}
