use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
                    "description": "Read a file from the workspace. Returns up to `limit` lines starting at line `start_line` (1-based). search_code_tool returns 1-based line numbers — pass them directly as start_line.\nIf output is truncated, the result is JSON with `content`, `truncated: true`, and `next_start_line`; continue from `next_start_line` when the current function/block or requested flow boundary is incomplete.\nNOTE: output lines are prefixed with `N: ` for display only — the actual file content does not contain these prefixes. Never include them in old_text when using patch_file_tool.",
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
                    "description": "List files/directories in the workspace. Only call this when directory contents are genuinely unknown and search_code_tool cannot help. Never use as the first step for a code explanation or task — call search_code_tool first.",
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
                    "description": "Search the workspace for symbols, function definitions, handlers, or RPC methods. Use this to ground your first search in concrete code tokens rather than broad prose.\n\n- Use `|` for multiple query alternatives: e.g. `handle_session_prompt|persist_session|ToolRegistry`.\n- Use `path` to scope to specific files or directories: e.g. `path: \"src/acp.rs\"` or `path: \"src/tools|src/acp.rs\"`.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string", "description": "Ripgrep query for code symbols, methods, or handlers. Use `|` for alternatives."},
                            "glob": {"type": "string", "description": "Optional glob filter (e.g. `*.rs`)."},
                            "path": {"type": "string", "description": "Optional path scope (e.g. `src/acp.rs`)."}
                        },
                        "required": ["query"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "find_file_tool",
                    "description": "Find files in the workspace matching a glob pattern. Use this to discover file paths when you know the name or extension but not the full path.\n\nExamples: `**/*.rs`, `src/acp*`, `**/Cargo.toml`. The pattern may contain `|` for multiple alternatives, e.g. `src/acp.rs|src/agent_loop.rs`.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "pattern": {"type": "string", "description": "Glob pattern to match files. Examples: `**/*.rs`, `src/acp*`, `**/Cargo.toml`, or `src/acp.rs|src/agent_loop.rs`."}
                        },
                        "required": ["pattern"]
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
                    "description": "Run a shell command inside the current workspace.\n\nUse this for git, rg, tests, builds, linting, Flutter/Dart/Node commands, and deploy scripts when needed.\nDestructive commands (sudo, shutdown, git reset --hard, etc.) are blocked at the system level and will always fail regardless of user request.",
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
                    "description": "Apply a targeted text patch to an existing file.\n\nUse this when you know the exact snippet to replace and want a safer scoped edit than rewriting the entire file.\nRead or search the file first so the patch target is precise. Keep patches small: replace a single expression, helper, or adjacent block instead of an entire function whenever possible.\nWhen adding code or tests, preserve neighboring blocks and insert adjacent to related code; do not replace an existing test/function unless the user explicitly asked for replacement.\nIMPORTANT: old_text must be verbatim file content. The `N: ` line-number prefixes shown in read_file_tool output are display-only and must never appear in old_text.",
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
                    "description": "Create and write a new file in the workspace.\n\nUse this whenever the user asks to create, make, write, save, or generate a file, note, document, config, or code artifact.\nAlways pass filename when the user names an exact file or path. Use the exact filename the user requested — do not invent or substitute a different path.\nDo not put only the desired file contents in instruction and omit filename.\nDo not answer with the file contents directly when this tool should be used.\nIf the user specified a length limit (\"short\", \"brief\", \"under N lines\"), include it in instruction.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "instruction": {"type": "string", "description": "What the file should contain."},
                            "filename": {"type": "string", "description": "Filename or relative path to create. Required when the user names an exact file or path."},
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
        let content = match fs::read_file(&self.workspace_cwd, path) {
            Ok(content) => content,
            Err(error) => {
                bail!(
                    "{}",
                    read_file_diagnostic_error(
                        path,
                        start_line,
                        limit,
                        &self.workspace_cwd,
                        &error.to_string(),
                    )
                );
            }
        };
        Ok(file_chunk_lines(&content, start_line, limit, Some(path)))
    }

    fn invoke_list_dir(&self, arguments: Map<String, Value>) -> Result<String> {
        let path = optional_string(&arguments, "path").unwrap_or(".");
        let entries = fs::list_dir(&self.workspace_cwd, path)?;
        Ok(entries.join("\n"))
    }

    fn invoke_search_code(&self, arguments: Map<String, Value>) -> Result<String> {
        let query = required_string(&arguments, "query")?;
        let glob = optional_string(&arguments, "glob");
        let path = optional_string(&arguments, "path");
        if let Some(path) = path.filter(|path| path.contains('|')) {
            let mut outputs = Vec::new();
            let mut saw_existing_path = false;
            for path in split_pipe_alternatives(path) {
                let scoped = self.workspace_cwd.join(path);
                if !scoped.exists() {
                    continue;
                }
                saw_existing_path = true;
                let output = fs::search_code(&self.workspace_cwd, query, glob, Some(path))?;
                outputs.push(format!("== {path} ==\n{output}"));
            }
            if saw_existing_path {
                return Ok(outputs.join("\n\n"));
            }
            bail!(
                "{}",
                search_code_diagnostic_error(query, glob, path, &self.workspace_cwd)
            );
        }
        if let Some(path) = path {
            let scoped = self.workspace_cwd.join(path);
            if !scoped.exists() {
                bail!(
                    "{}",
                    search_code_diagnostic_error(query, glob, path, &self.workspace_cwd)
                );
            }
        }
        fs::search_code(&self.workspace_cwd, query, glob, path)
    }

    fn invoke_find_file(&self, arguments: Map<String, Value>) -> Result<String> {
        let pattern = required_string(&arguments, "pattern")?;
        if pattern.contains('|') {
            let mut matches = Vec::new();
            for pattern in split_pipe_alternatives(pattern) {
                let output = fs::find_files(&self.workspace_cwd, pattern)?;
                if output == "No files found." {
                    continue;
                }
                for line in output.lines() {
                    if !matches.iter().any(|existing| existing == line) {
                        matches.push(line.to_owned());
                    }
                }
            }
            matches.sort();
            return Ok(if matches.is_empty() {
                "No files found.".to_owned()
            } else {
                matches.join("\n")
            });
        }
        fs::find_files(&self.workspace_cwd, pattern)
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
        command::run_command(&self.cmd_sessions, &self.workspace_cwd, cmd)
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
        let content = fs::read_file(&self.workspace_cwd, path)?;
        let occurrences = if old_text.is_empty() {
            0
        } else {
            content.matches(old_text).count()
        };
        if old_text.is_empty() {
            bail!(
                "{}",
                patch_diagnostic_error(
                    "patch_empty_old_text",
                    "patch_file_tool requires non-empty old_text",
                    path,
                    old_text,
                    replace_all,
                    &content,
                    occurrences,
                )
            );
        }
        if occurrences == 0 {
            bail!(
                "{}",
                patch_diagnostic_error(
                    "patch_target_not_found",
                    "Patch target not found in file",
                    path,
                    old_text,
                    replace_all,
                    &content,
                    occurrences,
                )
            );
        }
        if !replace_all && occurrences > 1 {
            bail!(
                "{}",
                patch_diagnostic_error(
                    "patch_target_ambiguous",
                    "Patch target appears multiple times; set replace_all=true or use a more specific old_text",
                    path,
                    old_text,
                    replace_all,
                    &content,
                    occurrences,
                )
            );
        }

        let written = fs::apply_patch(&self.workspace_cwd, path, old_text, new_text, replace_all)?;
        self.emit_progress(ToolProgressEvent::FileModified {
            path: written.clone(),
            status: "patched".to_owned(),
        });
        let removed = prefix_lines("- ", old_text);
        let added = prefix_lines("+ ", new_text);
        Ok(format!("Diff: {written}\n```\n{removed}\n{added}\n```"))
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
                .complete(&messages, &[], 3000, 0.0, None)
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
        let n = rewritten.lines().count();
        Ok(format!(
            "Updated: {written} ({n} lines)\n```\n{}\n```",
            file_preview(&rewritten)
        ))
    }

    async fn invoke_create_artifact(&self, arguments: Map<String, Value>) -> Result<String> {
        let instruction = required_string(&arguments, "instruction")?;
        let filename = optional_string(&arguments, "filename");
        let kind = optional_string(&arguments, "kind");

        self.emit_progress(ToolProgressEvent::Reasoning {
            summary: "Planning a new artifact before generating file contents.".to_owned(),
        });

        let mut resolved_kind = kind.map(str::to_owned);
        let final_name = if let Some(f) = filename {
            f.to_owned()
        } else if let Some(f) = exact_filename_from_instruction(instruction) {
            f
        } else if looks_like_file_content(instruction) {
            bail!(
                "create_artifact_tool requires filename when instruction is file content. Pass the exact filename requested by the user."
            );
        } else {
            let model = self.require_model()?;
            if resolved_kind.is_none() {
                resolved_kind = Some(infer_kind(model, instruction).await?);
            }
            infer_filename(
                model,
                instruction,
                resolved_kind.as_deref().unwrap_or("markdown"),
            )
            .await?
        };

        if filename.is_some() && looks_like_file_content(instruction) {
            self.emit_progress(ToolProgressEvent::Reasoning {
                summary: format!("Writing provided content to `{final_name}`."),
            });
            let content = sanitize_generated_file_content(instruction);
            let written = fs::write_file(&self.workspace_cwd, &final_name, &content)?;
            self.emit_progress(ToolProgressEvent::FileModified {
                path: written.clone(),
                status: "created".to_owned(),
            });
            let n = content.lines().count();
            return Ok(format!(
                "Created: {written} ({n} lines)\n```\n{}\n```",
                file_preview(&content)
            ));
        }

        let model = self.require_model()?;
        let resolved_kind = match resolved_kind {
            Some(k) => k,
            None => infer_kind(model, instruction).await?,
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
                .complete(&content_messages, &[], 3000, 0.1, None)
                .await?
                .content
                .unwrap_or_default(),
        );
        let written = fs::write_file(&self.workspace_cwd, &final_name, &content)?;
        self.emit_progress(ToolProgressEvent::FileModified {
            path: written.clone(),
            status: "created".to_owned(),
        });
        let n = content.lines().count();
        Ok(format!(
            "Created: {written} ({n} lines)\n```\n{}\n```",
            file_preview(&content)
        ))
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
            "find_file_tool" => self.invoke_find_file(arguments),
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

fn looks_like_file_content(instruction: &str) -> bool {
    let trimmed = instruction.trim_start();
    trimmed.starts_with('#')
        || trimmed.starts_with('{')
        || trimmed.starts_with('[')
        || trimmed.starts_with("```")
        || trimmed.lines().take(5).any(|line| line.starts_with("pub "))
}

fn sanitize_generated_file_content(text: &str) -> String {
    let mut cleaned = text.replace("<|im_end|>", "").replace("<|im_start|>", "");

    for (open, close) in [
        ("<thinking>", "</thinking>"),
        ("<think>", "</think>"),
        ("<|think|>", "<|/think|>"),
    ] {
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

    let drop_channel_lines = Regex::new(
        r"(?m)^\s*(?:<\|channel>thought|<\|channel>|<channel\|>|<\|turn\|>|<turn\|>|</turn>).*$",
    )
    .unwrap();
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

    cleaned = cleaned
        .replace("<channel|>", "")
        .replace("<|channel>", "")
        .replace("<|channel>thought", "")
        .replace("<|think|>", "")
        .replace("<|/think|>", "")
        .replace("<think>", "")
        .replace("</think>", "")
        .replace("<thinking>", "")
        .replace("</thinking>", "")
        .replace("<|turn|>", "")
        .replace("<turn|>", "")
        .replace("</turn>", "");

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
        r"(?i)\b([A-Za-z0-9][A-Za-z0-9._/-]*\.(?:md|txt|json|toml|ya?ml|rs|py|js|ts|tsx|jsx|dart|html|css))\b",
    )
    .unwrap();
    let filename = re.captures(instruction)?.get(1)?.as_str();
    Some(filename.trim_matches('`').to_owned())
}

fn split_pipe_alternatives(value: &str) -> impl Iterator<Item = &str> {
    value
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

fn suggest_workspace_path(cwd: &Path, requested: &str) -> Option<String> {
    let requested = requested.trim().trim_matches('/');
    if requested.is_empty() {
        return None;
    }

    let mut matches = Vec::new();
    collect_path_suffix_matches(cwd, cwd, requested, &mut matches, 0);
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

fn collect_path_suffix_matches(
    root: &Path,
    dir: &Path,
    requested: &str,
    matches: &mut Vec<String>,
    depth: usize,
) {
    if depth > 6 || matches.len() > 1 {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            ".git" | "target" | ".venv" | "venv" | "__pycache__"
        ) {
            continue;
        }

        if let Ok(relative) = path.strip_prefix(root) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.ends_with(requested) {
                matches.push(relative);
                if matches.len() > 1 {
                    return;
                }
            }
        }

        if path.is_dir() {
            collect_path_suffix_matches(root, &path, requested, matches, depth + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn file_chunk_lines(content: &str, start_line: usize, limit: usize, path: Option<&str>) -> String {
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
        let next_start_line = to + 1;
        let mut metadata = Map::new();
        if let Some(path) = path {
            metadata.insert("path".to_owned(), Value::String(path.to_owned()));
        }
        metadata.insert("content".to_owned(), Value::String(chunk.clone()));
        metadata.insert("start_line".to_owned(), json!(start_line));
        metadata.insert("end_line".to_owned(), json!(to));
        metadata.insert("total_lines".to_owned(), json!(total));
        metadata.insert("truncated".to_owned(), Value::Bool(true));
        metadata.insert("next_start_line".to_owned(), json!(next_start_line));
        metadata.insert(
            "continuation_hint".to_owned(),
            Value::String(format!(
                "Continue with read_file_tool start_line={next_start_line} if the current function, block, or requested flow boundary is incomplete."
            )),
        );
        serde_json::to_string_pretty(&Value::Object(metadata))
            .unwrap_or_else(|_| format!("{chunk}\n[Continue at {next_start_line}.]"))
    }
}

fn format_file_line(line_number: usize, line: &str) -> String {
    format!("{line_number}: {line}")
}

fn file_preview(content: &str) -> String {
    file_chunk_lines(content, 1, 80, None)
}

fn read_file_diagnostic_error(
    path: &str,
    start_line: usize,
    limit: usize,
    cwd: &Path,
    message: &str,
) -> String {
    let suggestion = suggest_workspace_path(cwd, path);
    json!({
        "code": "read_file_failed",
        "message": message,
        "diagnostics": {
            "path": path,
            "cwd": cwd.display().to_string(),
            "start_line": start_line,
            "limit": limit,
            "suggested_path": suggestion,
            "path_exists": cwd.join(path).exists(),
        }
    })
    .to_string()
}

fn search_code_diagnostic_error(query: &str, glob: Option<&str>, path: &str, cwd: &Path) -> String {
    let suggestion = suggest_workspace_path(cwd, path);
    json!({
        "code": "search_path_not_found",
        "message": format!("search_code_tool path not found: {path}"),
        "diagnostics": {
            "query": query,
            "glob": glob,
            "path": path,
            "cwd": cwd.display().to_string(),
            "suggested_path": suggestion,
            "path_exists": cwd.join(path).exists(),
        }
    })
    .to_string()
}

fn patch_diagnostic_error(
    code: &str,
    message: &str,
    path: &str,
    old_text: &str,
    replace_all: bool,
    content: &str,
    exact_occurrences: usize,
) -> String {
    let without_line_prefixes = strip_display_line_prefixes(old_text);
    let without_line_prefixes_occurrences = without_line_prefixes
        .as_deref()
        .map(|candidate| content.matches(candidate).count());
    let old_text_with_actual_newlines_occurrences = if old_text.contains("\\n") {
        Some(content.matches(&old_text.replace("\\n", "\n")).count())
    } else {
        None
    };
    let old_text_with_escaped_newlines_occurrences = if old_text.contains('\n') {
        Some(content.matches(&old_text.replace('\n', "\\n")).count())
    } else {
        None
    };
    let first_nonempty_line = old_text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let first_line_occurrence_lines = if first_nonempty_line.is_empty() {
        Vec::new()
    } else {
        lines_containing(content, first_nonempty_line, 10)
    };

    json!({
        "code": code,
        "message": message,
        "diagnostics": {
            "path": path,
            "replace_all": replace_all,
            "exact_occurrences": exact_occurrences,
            "exact_occurrence_start_lines": exact_match_start_lines(content, old_text, 10),
            "file": {
                "line_count": content.lines().count(),
                "char_count": content.chars().count(),
            },
            "old_text": {
                "line_count": old_text.lines().count(),
                "char_count": old_text.chars().count(),
                "contains_actual_newlines": old_text.contains('\n'),
                "contains_literal_backslash_n": old_text.contains("\\n"),
                "contains_display_line_prefixes": old_text_contains_display_line_prefixes(old_text),
            },
            "alternate_occurrences": {
                "without_display_line_prefixes": without_line_prefixes_occurrences,
                "with_actual_newlines": old_text_with_actual_newlines_occurrences,
                "with_escaped_newlines": old_text_with_escaped_newlines_occurrences,
            },
            "first_nonempty_old_text_line": first_nonempty_line,
            "first_line_occurrence_lines": first_line_occurrence_lines,
        }
    })
    .to_string()
}

fn exact_match_start_lines(content: &str, needle: &str, limit: usize) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    content
        .match_indices(needle)
        .take(limit)
        .map(|(byte_index, _)| line_number_at_byte(content, byte_index))
        .collect()
}

fn lines_containing(content: &str, needle: &str, limit: usize) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| line.contains(needle).then_some(index + 1))
        .take(limit)
        .collect()
}

fn line_number_at_byte(content: &str, byte_index: usize) -> usize {
    content[..byte_index]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn old_text_contains_display_line_prefixes(old_text: &str) -> bool {
    old_text.lines().any(starts_with_display_line_prefix)
}

fn strip_display_line_prefixes(old_text: &str) -> Option<String> {
    if !old_text_contains_display_line_prefixes(old_text) {
        return None;
    }
    Some(
        old_text
            .lines()
            .map(|line| {
                if starts_with_display_line_prefix(line) {
                    line.split_once(": ").map(|(_, rest)| rest).unwrap_or(line)
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn starts_with_display_line_prefix(line: &str) -> bool {
    let Some((number, _)) = line.trim_start().split_once(": ") else {
        return false;
    };
    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn prefix_lines(prefix: &str, text: &str) -> String {
    text.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
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
        BuiltinToolRegistry, exact_filename_from_instruction, fallback_filename, infer_extension,
        sanitize_generated_file_content, slugify,
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
                "find_file_tool",
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
    fn exact_filename_from_instruction_preserves_relative_paths() {
        assert_eq!(
            exact_filename_from_instruction(
                "Create a short plan in HEALTH_SANDBOX/portfolio-plan.md."
            ),
            Some("HEALTH_SANDBOX/portfolio-plan.md".to_owned())
        );
    }

    #[tokio::test]
    async fn patch_file_tool_reports_structured_target_diagnostics() {
        let tempdir = TempDir::new().expect("tempdir");
        stdfs::write(tempdir.path().join("notes.txt"), "alpha\nbeta\ngamma\n").expect("write");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");

        let err = registry
            .invoke(
                "patch_file_tool",
                json!({
                    "path": "notes.txt",
                    "old_text": "1: alpha\n2: beta",
                    "new_text": "alpha\nBETTER",
                })
                .as_object()
                .cloned()
                .unwrap(),
            )
            .await
            .expect_err("line-number-prefixed old_text should not match");

        let parsed: serde_json::Value =
            serde_json::from_str(&err.to_string()).expect("structured patch error");
        assert_eq!(parsed["code"], "patch_target_not_found");
        assert_eq!(parsed["message"], "Patch target not found in file");
        assert_eq!(parsed["diagnostics"]["path"], "notes.txt");
        assert_eq!(parsed["diagnostics"]["exact_occurrences"], 0);
        assert_eq!(
            parsed["diagnostics"]["alternate_occurrences"]["without_display_line_prefixes"],
            1
        );
        assert_eq!(
            parsed["diagnostics"]["old_text"]["contains_display_line_prefixes"],
            true
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
<|think|>hidden<|/think|>
<|turn|>
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

        assert!(result.starts_with("Updated:"), "result: {result}");
        assert!(result.contains("notes.txt"), "result: {result}");
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

        assert!(result.starts_with("Created:"), "result: {result}");
        assert!(result.contains("doc.md"), "result: {result}");
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

        assert!(result.starts_with("Created:"), "result: {result}");
        assert!(result.contains("my-note.md"), "result: {result}");
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

        assert!(result.starts_with("Created:"), "result: {result}");
        assert!(result.contains("invalid.txt"), "result: {result}");
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

        assert!(result.starts_with("Created:"), "result: {result}");
        assert!(result.contains("CONTRIBUTING.md"), "result: {result}");
        assert!(tempdir.path().join("CONTRIBUTING.md").exists());
    }

    #[tokio::test]
    async fn create_artifact_tool_writes_exact_content_with_filename_without_model() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        let content = "# Plan\n\n- Build\n- Test\n";

        let result = registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": content, "filename": "plan.md", "kind": "markdown"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect("invoke");

        assert!(result.starts_with("Created:"), "result: {result}");
        assert_eq!(
            stdfs::read_to_string(tempdir.path().join("plan.md")).unwrap(),
            content
        );
    }

    #[tokio::test]
    async fn create_artifact_tool_rejects_content_only_instruction_without_filename() {
        let tempdir = TempDir::new().expect("tempdir");
        let model = Arc::new(MockModel::new(vec!["markdown"]));
        let registry =
            BuiltinToolRegistry::new_with_model(tempdir.path(), model).expect("registry");

        let err = registry
            .invoke(
                "create_artifact_tool",
                json!({"instruction": "# Contributing\n\nRun `cargo test`."})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
            .await
            .expect_err("content-only instruction should require filename");

        assert!(err.to_string().contains("requires filename"));
        assert!(!tempdir.path().join("contributing-guide.md").exists());
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
        let tool_result = &result.tool_results[0].result;
        assert!(tool_result.starts_with("Created:"), "result: {tool_result}");
        assert!(tool_result.contains("summary.md"), "result: {tool_result}");
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
        let page1_json: serde_json::Value = serde_json::from_str(&page1).expect("page 1 metadata");
        assert_eq!(page1_json["start_line"], 1);
        assert_eq!(page1_json["end_line"], 200);
        assert_eq!(page1_json["total_lines"], 450);
        assert_eq!(page1_json["truncated"], true);
        assert_eq!(page1_json["next_start_line"], 201);
        assert!(
            page1_json["content"]
                .as_str()
                .expect("content")
                .contains("200: line 200"),
            "page 1 should include numbered content: {page1}"
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
        let page2_json: serde_json::Value = serde_json::from_str(&page2).expect("page 2 metadata");
        assert_eq!(page2_json["start_line"], 201);
        assert_eq!(page2_json["end_line"], 400);
        assert_eq!(page2_json["truncated"], true);
        assert_eq!(page2_json["next_start_line"], 401);

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

    #[test]
    fn read_file_tool_suggests_unique_suffix_match_for_missing_path() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        stdfs::create_dir_all(tempdir.path().join("src/tools")).expect("mkdir");
        stdfs::write(
            tempdir.path().join("src/tools/mod.rs"),
            "fn file_chunk_lines() {}\n",
        )
        .expect("write");

        let err = futures::executor::block_on(
            registry.invoke(
                "read_file_tool",
                json!({"path": "tools/mod.rs"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .expect_err("missing path should error with suggestion");

        let parsed: serde_json::Value =
            serde_json::from_str(&err.to_string()).expect("structured read error");
        assert_eq!(parsed["code"], "read_file_failed");
        assert_eq!(parsed["diagnostics"]["path"], "tools/mod.rs");
        assert_eq!(parsed["diagnostics"]["suggested_path"], "src/tools/mod.rs");
        assert_eq!(parsed["diagnostics"]["path_exists"], false);
    }

    #[test]
    fn search_code_tool_reports_structured_missing_path_diagnostics() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        stdfs::create_dir_all(tempdir.path().join("src/tools")).expect("mkdir");
        stdfs::write(tempdir.path().join("src/tools/mod.rs"), "fn run() {}\n").expect("write");

        let err = futures::executor::block_on(
            registry.invoke(
                "search_code_tool",
                json!({"query": "run", "path": "tools/mod.rs"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .expect_err("missing scoped path should error with diagnostics");

        let parsed: serde_json::Value =
            serde_json::from_str(&err.to_string()).expect("structured search error");
        assert_eq!(parsed["code"], "search_path_not_found");
        assert_eq!(parsed["diagnostics"]["query"], "run");
        assert_eq!(parsed["diagnostics"]["path"], "tools/mod.rs");
        assert_eq!(parsed["diagnostics"]["suggested_path"], "src/tools/mod.rs");
    }

    #[test]
    fn search_code_tool_accepts_pipe_separated_paths() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        stdfs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
        stdfs::write(tempdir.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        stdfs::write(
            tempdir.path().join("src/acp.rs"),
            "fn handle_message() {}\n",
        )
        .expect("write");

        let result = futures::executor::block_on(
            registry.invoke(
                "search_code_tool",
                json!({"query": "fn", "path": "src/main.rs|src/acp.rs"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .expect("pipe-separated scoped paths should search both files");

        assert!(result.contains("== src/main.rs =="), "{result}");
        assert!(result.contains("== src/acp.rs =="), "{result}");
        assert!(result.contains("fn main"), "{result}");
        assert!(result.contains("fn handle_message"), "{result}");
    }

    #[test]
    fn find_file_tool_accepts_pipe_separated_patterns() {
        let tempdir = TempDir::new().expect("tempdir");
        let registry = BuiltinToolRegistry::new(tempdir.path()).expect("registry");
        stdfs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
        stdfs::write(tempdir.path().join("src/acp.rs"), "").expect("write");
        stdfs::write(tempdir.path().join("src/agent_loop.rs"), "").expect("write");

        let result = futures::executor::block_on(
            registry.invoke(
                "find_file_tool",
                json!({"pattern": "src/acp.rs|src/agent_loop.rs"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .expect("pipe-separated patterns should be supported");

        assert!(result.contains("src/acp.rs"), "{result}");
        assert!(result.contains("src/agent_loop.rs"), "{result}");
    }
}
