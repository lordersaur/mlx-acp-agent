# mlx-acp-agent

A Codex/Claude Code-style local coding agent written in Rust, speaking ACP (Agent Control Protocol) JSON-RPC 2.0 over stdio — used by the Zed editor.

**Model:** `mlx-community/gemma-4-e4b-it-4bit` on M1 Pro 16GB — OmniCoder is no longer the target model.  
**Fine-tuning machine:** MacBook Pro M1 Pro with 16GB RAM — LoRA model creation and experiments are planned around this local hardware constraint.  
**Binary:** `target/release/rust-agent`  
**MLX server:** `~/python-mlx-sv/main.py` (FastAPI, runs at `http://127.0.0.1:8000`)

## Quick Start

```bash
# Start MLX server
source ~/mlx-env/bin/activate
cd ~/python-mlx-sv && uvicorn main:app

# Build agent
cargo build --release

# Run tests
cargo test
```

## Repo Structure

```
src/
  main.rs            — binary entrypoint
  lib.rs             — crate root
  acp.rs             — ACP JSON-RPC 2.0 server; ClientCaller for bidirectional RPC; ProgressRegistry with terminal support
  agent_loop.rs      — core tool-calling loop, SYSTEM_PROMPT, ModelClient / ToolExecutor traits
  mlx_client.rs      — HTTP client for /v1/chat/completions; ChatMessage with tool_calls/tool_call_id; streaming think-tag state machine
  model_parser.rs    — thought/channel token stripping (no tool parsing)
  config.rs          — AppConfig: MLX_URL, MLX_MODEL env vars
  session_store.rs   — TurnRecord session history, disk persistence, 7-day prune
  tools/
    mod.rs           — BuiltinToolRegistry (15 tools); ProgressRegistry wraps it with ACP events; read_file_tool uses 1-based line numbers (start_line, limit in lines)
    fs.rs            — read_file, list_dir, search_code, patch_file, delete_path, edit_file, create_artifact
    command.rs       — run_command (60s timeout), command sessions (start/read/write/terminate)
    web.rs           — web_fetch, web_search (DuckDuckGo)

~/python-mlx-sv/main.py  — MLX FastAPI server (separate repo)
```

## Tools (15)

`read_file_tool`, `list_dir_tool`, `search_code_tool`, `web_search_tool`, `web_fetch_tool`,
`run_command_tool`, `start_command_session_tool`, `list_command_sessions_tool`,
`read_command_session_tool`, `write_command_session_tool`, `terminate_command_session_tool`,
`patch_file_tool`, `delete_path_tool`, `edit_file_tool`, `create_artifact_tool`

## ACP Methods (7)

`initialize`, `authenticate`, `session/new`, `session/load`, `session/prompt`,
`session/set_mode`, `session/cancel`

## Modes (Zed panel dropdown)

| Mode | Description |
|------|-------------|
| Ask | Read-only — answers questions and inspects code |
| Edit | File changes only — read, patch, create |
| Agent | Full mode — search, web, shell, edits, validation |
| Fast | Agent mode with thinking disabled (`/no_think`) — faster responses |

## Config

- `MLX_URL` — defaults to `http://127.0.0.1:8000/v1/chat/completions`
- `MLX_MODEL` — defaults to `mlx-community/Qwen3.5-9B-OptiQ-4bit` (stale aliases rejected by server, falls back to default)

## Phase Status

| Phase | Description | Status |
|-------|-------------|--------|
| 0 | Switch model, validate tool calling | ✅ Done |
| 1 | Core agent loop | ✅ Done |
| 2 | Simplified session state | ✅ Done |
| 3 | Simplified routing | ✅ Done |
| 4 | Internet access (web_fetch, web_search) | ✅ Done |
| 5 | Terminal session upgrade | ✅ Done |
| 6 | Persistent chat and task history | ✅ Done |
| 7 | Native tool calling via OpenAI tools API | ✅ Done |
| 8 | System prompt refinement + context truncation | ✅ Done |
| 9 | ACP terminal support (live Zed widget) | ✅ Done |
| 10 | Conversation history UI (like Claude Code / Codex) | 🔲 Next |
| 11 | Subagents — `delegate_task_tool(task, tools, max_steps)` | 🔲 Planned |
| 12 | LoRA fine-tuning pipeline for the local agent model | 🔲 Planned |
| 12 | LoRA fine-tuning pipeline for the local agent model | 🔲 Planned |

**Next:** Phase 10 — Conversation history panel in Zed showing past sessions with timestamps, searchable turns, and resume support (like `/history` in Claude Code or Codex's session list).

### Phase 12 — LoRA Fine-tuning (design)
Goal: create a local LoRA fine-tuning workflow for improving the agent model on real coding-agent traces, constrained to the available MacBook Pro M1 Pro with 16GB RAM.

Key pieces:
- **Dataset extraction** — convert successful session history from `~/.mlx-acp-agent/history/` into training examples, preserving user prompts, assistant answers, tool calls, and tool results where useful
- **Data cleaning** — remove secrets, local credentials, irrelevant command noise, failed tool loops, and low-quality turns before training
- **Training format** — produce chat/tool-calling examples compatible with the MLX/Qwen chat template used by `~/python-mlx-sv/main.py`
- **LoRA training** — run MLX-compatible LoRA fine-tuning locally with conservative batch size, sequence length, and adapter rank settings suitable for 16GB unified memory
- **Evaluation set** — keep a small held-out set of agent tasks to compare baseline vs LoRA behavior before adopting the adapter
- **Adapter loading** — update the MLX server config to load the selected LoRA adapter without replacing the base quantized model
- **Rollback path** — allow disabling the LoRA adapter quickly if tool calling, formatting, or instruction following regresses

### Phase 10 — Conversation History (design)
Goal: surface past sessions and turns inside the Zed ACP panel, similar to how Claude Code shows prior conversation history and Codex lists past task runs.

Key pieces:
- **Session list** — expose all sessions from `~/.mlx-acp-agent/history/` via a new ACP method or by returning metadata in `session/load`
- **Turn browser** — each `TurnRecord` (prompt, answer, tool_results, timestamps) rendered as a collapsible turn in the panel
- **Search** — filter turns by keyword across sessions (`search_code`-style, but over history JSON)
- **Resume** — `session/load` already exists; add UI affordance to click a past session and continue it
- **Export** — dump a session to markdown (already partially done via `to_markdown` helpers)

### Phase 12 — LoRA Fine-tuning (design)
Goal: create a local LoRA fine-tuning workflow for improving the agent model on real coding-agent traces, constrained to the available MacBook Pro M1 Pro with 16GB RAM.

Key pieces:
- **Dataset extraction** — convert successful session history from `~/.mlx-acp-agent/history/` into training examples, preserving user prompts, assistant answers, tool calls, and tool results where useful
- **Data cleaning** — remove secrets, local credentials, irrelevant command noise, failed tool loops, and low-quality turns before training
- **Training format** — produce chat/tool-calling examples compatible with the MLX/Qwen chat template used by `~/python-mlx-sv/main.py`
- **LoRA training** — run MLX-compatible LoRA fine-tuning locally with conservative batch size, sequence length, and adapter rank settings suitable for 16GB unified memory
- **Evaluation set** — keep a small held-out set of agent tasks to compare baseline vs LoRA behavior before adopting the adapter
- **Adapter loading** — update the MLX server config to load the selected LoRA adapter without replacing the base quantized model
- **Rollback path** — allow disabling the LoRA adapter quickly if tool calling, formatting, or instruction following regresses

## Key Architecture Notes

### Message flow
```
Zed → ACP session/prompt
  → acp.rs builds Vec<ConversationMessage> from history
  → run_agent_loop converts to Vec<ChatMessage> + prepends SYSTEM_PROMPT
  → loop: model.complete(messages, tool_schemas) → CompletionResult
  → if tool_calls: push assistant_with_tool_calls + role:"tool" results
  → if text: return LoopResult { answer, tool_results, iterations }
  → acp.rs persists turn, sends agent_message_chunk
```

### Agent loop limits
- Max 15 iterations per prompt
- Max 3 parallel tool calls per turn (excess dropped, re-requested next iteration)
- History window: last 10 turns × 2 messages kept in conversation

### Fast mode / thinking toggle
When mode is `fast`, `build_messages` in `acp.rs` prepends `/no_think` to the system message.
`main.py` detects this and passes `enable_thinking=False` to `apply_chat_template`, skipping
the `<think>` block entirely. Switching back to `agent` mode re-enables thinking.

### ACP terminal support (Phase 9)
When Zed advertises `clientCapabilities.terminal = true`, `run_command_tool` routes
through `invoke_via_terminal` instead of the local subprocess runner:
1. `terminal/create` — opens a live terminal widget in the Zed panel
2. `terminal/wait_for_exit` — blocks until the command exits (5-min client-side timeout)
3. `terminal/output` — collects captured stdout
4. `terminal/release` — cleanup

All three post-create calls require both `sessionId` and `terminalId` in the request.
Falls back to local `run_command_tool` if any step fails.

### Qwen3.5 tool calling quirks
- `role: "tool"` messages must be converted to `role: "user"` with `<tool_response>` wrapper before reaching the Jinja2 template (`message_to_dict` in `main.py`)
- `arguments` in tool_calls must be parsed from JSON string to dict before template sees it
- Model uses `<think>…</think>` tags with pre-filled thinking: `apply_chat_template` with `enable_thinking=True` injects `<think>\n` into the generation prompt, so model output begins with reasoning and ends with `</think>`. `stream_think_chunk` in `mlx_client.rs` handles this "pre-filled" pattern.
- `extract_post_think` in `mlx_client.rs` strips everything up to `</think>` or `</thinking>` before returning the answer
- 4-stage tool call extraction in `main.py`: `standard_xml_json` → `xml_function_parameters` → `qwen_hybrid_json` → `qwen_compact_native`
- `_normalize_qwen_args` handles Qwen double-brace syntax `{{…}}` and quote normalization (`<|"|>` → `"`)
- Template fallback order: drop `enable_thinking` first (keeps tools), then drop `tools` if still failing

### Context truncation (main.py)
`truncate_messages` keeps: system + last user message (anchor) + fills backwards from most recent.
Prevents OOM on long sessions. Anchors on the *most recent* user message to avoid resurrecting stale tasks.
Max context: 60,000 chars total, 45,000 chars for the anchor window.

### Persistence
- Session history: `~/.mlx-acp-agent/history/<session_id>.json`
- Command sessions: `~/.mlx-acp-agent/sessions/<cwd_hash>/`
- Retention: 7-day prune on startup

## Known Bugs (observed April 2026)

### Bug 3 — read_file_tool char offset vs line number mismatch (HIGH) ✅ Fixed
**Symptom:** `search_code_tool` returns 1-based line numbers, but `read_file_tool` used character-based `offset`. Model passed line 422 as `offset=422` (char 422 ≈ file start), saw the same content repeatedly, hit 15-iteration max.  
**Fix:** `read_file_tool` now takes `start_line` (1-based) and `limit` (lines, default 100). Output includes line numbers (`N: content`) matching search results. The `file_chunk_lines` helper in `tools/mod.rs` implements this.

### Bug 2 — Tool call injection from web content (HIGH) ✅ Fixed
**Symptom:** After `web_fetch_tool` returns a page that contains `<function=…>` XML, the parser fires on that content and executes the embedded examples as real tool calls.  
**Fix:** `message_to_dict` in `main.py` escapes `<function=`, `<tool_call>`, `</tool_call>` sequences in tool results before inserting into conversation.

## Tests

72 total — all pass. Run: `cargo test`
