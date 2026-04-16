# mlx-acp-agent

A Codex/Claude Code-style local coding agent written in Rust, speaking ACP (Agent Control Protocol) JSON-RPC 2.0 over stdio — used by the Zed editor.

**Model:** `mlx-community/Qwen3-14B-4bit` on M1 Pro 16GB  
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
  mlx_client.rs      — HTTP client for /v1/chat/completions; ChatMessage with tool_calls/tool_call_id
  model_parser.rs    — thought/channel token stripping only (no tool parsing)
  config.rs          — AppConfig: MLX_URL, MLX_MODEL env vars
  session_store.rs   — TurnRecord session history, disk persistence, 7-day prune
  tools/
    mod.rs           — BuiltinToolRegistry (15 tools); ProgressRegistry wraps it with ACP events
    fs.rs            — read_file, list_dir, search_code, patch_file, delete_path
    command.rs       — run_command (60s timeout), command sessions (start/read/write/terminate)
    web.rs           — web_fetch, web_search (DuckDuckGo)

~/python-mlx-sv/main.py  — MLX FastAPI server (separate repo)
```

## Tools (15)

`read_file`, `list_dir`, `search_code`, `web_search`, `web_fetch`, `run_command`,
`start_command_session`, `list_command_sessions`, `read_command_session`,
`write_command_session`, `terminate_command_session`, `patch_file`, `delete_path`,
`edit_file`, `create_artifact`

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
- `MLX_MODEL` — defaults to `mlx-community/Qwen3.5-9B-OptiQ-4bit`

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

**Next:** Phase 10 — Conversation history panel in Zed showing past sessions with timestamps, searchable turns, and resume support (like `/history` in Claude Code or Codex's session list).

### Phase 10 — Conversation History (design)
Goal: surface past sessions and turns inside the Zed ACP panel, similar to how Claude Code shows prior conversation history and Codex lists past task runs.

Key pieces:
- **Session list** — expose all sessions from `~/.mlx-acp-agent/history/` via a new ACP method or by returning metadata in `session/load`
- **Turn browser** — each `TurnRecord` (prompt, answer, tool_results, timestamps) rendered as a collapsible turn in the panel
- **Search** — filter turns by keyword across sessions (`search_code`-style, but over history JSON)
- **Resume** — `session/load` already exists; add UI affordance to click a past session and continue it
- **Export** — dump a session to markdown (already partially done via `to_markdown` helpers)

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

### Qwen3 tool calling quirks
- `role: "tool"` messages must be converted to `role: "user"` with `<tool_response>` wrapper before reaching the Jinja2 template (`message_to_dict` in `main.py`)
- `arguments` in tool_calls must be parsed from JSON string to dict before template sees it
- Model sometimes outputs orphaned `</think>` tags — `clean_output` in `main.py` strips these
- Think content must be stripped from assistant history messages — only post-think text is stored

### Context truncation (main.py)
`truncate_messages` keeps: system + last user message (anchor) + fills backwards from most recent.
Prevents OOM on long sessions. Anchors on the *most recent* user message to avoid resurrecting stale tasks.

## Known Bugs (observed April 2026)

### Bug 1 — Truncation resurrects wrong task (CRITICAL)
**Symptom:** Agent abandons the current task mid-way and restarts a completely different, earlier task.  
**Root cause:** `truncate_messages` always keeps the *first* non-system user message as an anchor. In a multi-turn session the first message is the opening task (e.g. `cargo test`), not the current one. After heavy truncation (`kept_turns=0`) the model only sees system + that stale first message and believes it is starting fresh on that task.  
**Fix (main.py `truncate_messages`):** Anchor on the *most recent* user message, not the first.

### Bug 2 — Tool call injection from web content (HIGH)
**Symptom:** After `web_fetch_tool` returns a page that contains `<function=…>` XML, the parser fires on that content and executes the embedded examples as real tool calls.  
**Fix (main.py):** Escape `<function=`, `<tool_call>`, `</tool_call>` sequences in tool results before inserting into conversation. ✅ Done for `role: "tool"` messages via `message_to_dict`.

### Persistence
- Session history: `~/.mlx-acp-agent/history/<session_id>.json`
- Command sessions: `~/.mlx-acp-agent/sessions/<cwd_hash>/`
- Retention: 7-day prune on startup

## Tests

72 total — all pass. Run: `cargo test`
