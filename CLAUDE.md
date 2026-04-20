# mlx-acp-agent

A Codex/Claude Code-style local coding agent written in Rust, speaking ACP (Agent Control Protocol) JSON-RPC 2.0 over stdio — used by the Zed editor.

**Model:** `mlx-community/gemma-4-e4b-it-OptiQ-4bit` on M1 Pro 16GB
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
    mod.rs           — BuiltinToolRegistry (16 tools); ProgressRegistry wraps it with ACP events; read_file_tool uses 1-based line numbers (start_line, limit in lines)
    fs.rs            — read_file, list_dir, find_files, search_code, patch_file, delete_path, edit_file, create_artifact
    command.rs       — run_command (60s timeout), command sessions (start/read/write/terminate)
    web.rs           — web_fetch, web_search (DuckDuckGo)

~/python-mlx-sv/main.py  — MLX FastAPI server (separate repo)
```

## Tools (16)

`read_file_tool`, `list_dir_tool`, `search_code_tool`, `find_file_tool`,
`web_search_tool`, `web_fetch_tool`, `run_command_tool`, `start_command_session_tool`,
`list_command_sessions_tool`,
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
- `MLX_MODEL` — Rust config accepts explicit model names; MLX server defaults stale aliases to `mlx-community/gemma-4-e4b-it-OptiQ-4bit`

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
| 13 | Rich tool-call input envelopes | 🔲 Planned |

**Next:** Phase 10 — Conversation history panel in Zed showing past sessions with timestamps, searchable turns, and resume support (like `/history` in Claude Code or Codex's session list).

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
- **Training format** — produce chat/tool-calling examples compatible with the MLX chat template used by `~/python-mlx-sv/main.py`
- **LoRA training** — run MLX-compatible LoRA fine-tuning locally with conservative batch size, sequence length, and adapter rank settings suitable for 16GB unified memory
- **Evaluation set** — keep a small held-out set of agent tasks to compare baseline vs LoRA behavior before adopting the adapter
- **Adapter loading** — update the MLX server config to load the selected LoRA adapter without replacing the base quantized model
- **Rollback path** — allow disabling the LoRA adapter quickly if tool calling, formatting, or instruction following regresses

### Phase 13 — Rich Tool-call Input Envelopes (design)
Goal: add structured metadata for tool invocations, similar to Codex unified exec input,
without changing tool behavior or replacing model reasoning.

Key pieces:
- **Invocation envelope** — record `call_id`, `turn_id`, tool name, raw arguments,
  workspace cwd, source, and timestamps for every tool call.
- **Parsed intent** — derive factual metadata such as operation type (`read`, `search`,
  `write`, `execute`), target path/name, command summary, and affected workspace scope.
- **Persistence** — store invocation metadata alongside `ToolExecution` so session history
  can explain what the model attempted, not only what the tool returned.
- **Model exposure decision** — start internal-only; expose selected input metadata to the
  model only if health tests show it improves recovery or reduces repeated bad calls.
- **No hardcoded guardrails** — input metadata should be factual context. Tools expose facts;
  the model decides whether to continue, retry differently, or stop.

## Key Architecture Notes

### No hardcoding rule
- Do not fix health-test failures by adding prompt-specific function names, file names,
  canned final answers, or phrase lists.
- Prefer general mechanisms: richer factual tool output, structured diagnostics, source-backed
  traversal rules, sandbox enforcement, and tests that verify behavior without depending on one
  exact prompt.

### Flow tracing rule
- For questions about how a flow, lifecycle, protocol method, request, event, or persistence
  path works, the agent should derive the call chain from source: first convert the user's
  requested boundaries into searchable source tokens such as exact strings, protocol methods,
  routes, events, identifiers, file/path fragments, state/write terms, and likely
  handler/action words. Do not start with broad prose labels.
- After finding candidates, read the external entry point, follow local calls that transform
  input / invoke the core operation / emit output / persist state, then answer only from
  functions it actually read.
- Every hop named in the final flow must be backed by a source read of the function or block
  that proves that hop. Search results can identify candidates, but do not prove the call chain.
- Do not stop at the first plausible core function. If the read window ends before the current
  function's post-call handling is visible, continue reading; cleanup, output emission, state
  mutation, and persistence often happen after awaited calls or core-loop returns.
- Before answering, verify that the requested end boundary is backed by source that was read. If
  the last relevant read was truncated and the boundary is not visible, continue from the
  tool-provided `next_start_line` instead of inferring the ending.
- Do not describe handoffs with phrases like "ultimately passed to" unless the exact call site,
  emitted event, state mutation, or write was read in source.
- This rule must stay generic. Do not add ACP-specific function-name lists or canned message
  flow answers to make one health prompt pass.

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
- Tool results sent back to the model use a structured JSON envelope for every tool:
  `tool`, `status`, `input`, `error`, and `output`. `ToolExecution.result` stays raw for
  UI/session persistence; only the model-facing `role:"tool"` message is wrapped.
- Structured tool errors are opt-in by tool: if a tool returns JSON with `code`,
  `message`, or `diagnostics`, `run_agent_loop` embeds that object under
  `error`; plain string failures fall back to `{code:"tool_failed", message:...}`.
- Repeated identical failed tool calls are blocked after two failures and returned as a
  structured failed tool result so the model can choose a different approach.

### Tool diagnostics
Goal: add richer, computed diagnostics inside the common tool-result envelope without
hardcoded final-answer rewriting.

Implemented:
- `patch_file_tool` — target failures return structured diagnostics with occurrence
  counts, exact-match line starts, display line-prefix detection, newline/escape
  alternate occurrence counts, and first-line occurrence anchors.
- `read_file_tool` — missing-path failures return structured diagnostics with cwd,
  requested path, requested line window, path existence, and unique suffix suggestion.
- `search_code_tool` — missing scoped `path` failures return structured diagnostics with
  query, glob, cwd, path existence, and unique suffix suggestion.
- `search_code_tool` / `find_file_tool` — `path` / `pattern` values can contain `|`
  for multiple explicit scopes, e.g. `src/main.rs|src/acp.rs`.
- `create_artifact_tool` — if the caller provides both exact file content and an explicit
  filename, writes that content directly instead of asking the model to regenerate it.
  Content-only instructions without a filename are rejected.

Planned:
- `find_file_tool` — include cwd and pattern metadata on failure or empty results.
- Command tools — include command, cwd, exit code, stdout/stderr tails, running state, and
  session id consistently.
- Web tools — include URL, status code, redirect/fetch failure details, and content type.

Design rule: tools should expose factual diagnostics; the model should reason from those
facts and decide whether to retry, switch tools, or stop. Avoid English phrase-list
guardrails that rewrite final answers.

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

### ACP tool-call UI
- Tool-call start events include `rawInput` for clients, but visible panel text stays
  concise and tool-specific instead of dumping JSON arguments.
- Tool-call completion/error updates use Codex-like visible bodies:
  - search/read/list/find render one JSON block with `tool`, `input`, `status`, and output/error
  - patch/edit/create/delete render the tool's diff/preview block directly
  - command/session tools render a `Terminal:` block
- Completion/error updates set `rawOutput`; JSON outputs and structured diagnostic errors are
  parsed as JSON instead of opaque strings.
- Tool titles summarize intent, e.g. `Read src/acp.rs (10 - 29)` or
  `Search run_agent_loop in src/agent_loop.rs`.

### Model tool calling (main.py)
- `MODEL_FAMILY` switches behavior: Gemma gets native `role: "tool"` messages with
  `tool_call_id`; non-Gemma models still receive `role: "user"` with `<tool_response>`.
- `arguments` in tool_calls are parsed from JSON string to dict before the template sees them
- Gemma-native `<|tool_call>call:name{args}<tool_call|>` calls are parsed by a brace-counting
  scanner so nested `{}` inside patch strings do not break extraction.
- `<|"|>...<|"|>` delimited Gemma argument values preserve literal `\n` / `\t` / `\r`
  source escapes. Regular quoted values still decode control escapes.
- `clean_output` removes `<think>`, `<|channel>thought`, `<turn|>`, and stray channel tokens
  before tool-call extraction/final output.
- Qwen-style models use `<think>…</think>` tags with pre-filled thinking:
  `apply_chat_template` with `enable_thinking=True` injects `<think>\n` into the generation prompt
- `stream_think_chunk` in `mlx_client.rs` handles the "pre-filled" thinking pattern
- `extract_post_think` in `mlx_client.rs` strips everything up to `</think>` or `</thinking>` before returning the answer
- Tool call extraction in `main.py`: `gemma_native` first, then XML/compact fallbacks
  (`standard_xml_json`, `xml_function_parameters`, `qwen_hybrid_json`, `qwen_compact_native`)
- Template fallback order: drop `enable_thinking` first (keeps tools), then drop `tools` if still failing

### Gemma 4 System Prompt Optimization
Gemma 4 community tips for improved instruction adherence, especially with OptiQ 4-bit quantization:

- **Positive Rules:** Gemma 4 prioritizes positive instructions over negative ones. Instead of "Never answer from memory," use "Always use tools to gather information."
- **Context "Pull" Mitigation:** As the context window fills, the model prioritizes recent user tokens over system rules. `build_messages` in `acp.rs` automatically appends a rule reminder to the user's prompt after 5 turns.
- **Brevity:** Keep the system prompt short and absolute (under 500 words). Avoid complex "persona" descriptions.
- **No LaTeX:** Use plain text arrows (`->`) as Gemma 4 sometimes hallucinates LaTeX formatting (`$\rightarrow$`) which breaks ACP parsing.

### Context truncation (main.py)
`truncate_messages` keeps: system + last user message (anchor) + fills backwards from most recent.
Prevents OOM on long sessions. Anchors on the *most recent* user message to avoid resurrecting stale tasks.
Max context: 100,000 chars total, 75,000 chars for the anchor window.

### Persistence
- Session history: `~/.mlx-acp-agent/history/<session_id>.json`
- Command sessions: `~/.mlx-acp-agent/sessions/<cwd_hash>/`
- Retention: 7-day prune on startup

## Health Tests

Test files for verifying agent behavior without risking real implementation files:

- `agent-health-test-prompts.md` — main health suite; uses `HEALTH_SANDBOX/fixture-crate` for source-editing prompts
- `portfolio-project-agent-test.md` — end-to-end project test; do not run until health suite is passing

### Health preflight

Run this before another health prompt pass:

```bash
cargo test
cargo test --manifest-path HEALTH_SANDBOX/fixture-crate/Cargo.toml
rustfmt --edition 2021 --check src/agent_loop.rs src/tools/mod.rs
python3 -m py_compile ~/python-mlx-sv/main.py
```

Reset the health sandbox before validating fixture behavior. Some health prompts
intentionally leave `HEALTH_SANDBOX/fixture-crate` in a bad intermediate state so the
next prompt can test recovery behavior.

MLX server smoke check:

```bash
source ~/mlx-env/bin/activate
cd ~/python-mlx-sv && uvicorn main:app
```

Expected server log indicators during health tests:
- `tools_in_request=16`
- `extract_tool_calls matched=gemma_native` for Gemma tool calls
- `finish_reason=tool_calls` when a tool call is emitted
- no repeated identical `patch_file_tool` call beyond two failed attempts

### Health test safety rules

Prompts that mutate files must only touch `HEALTH_*` or `HEALTH_SANDBOX/**`. They must not modify:

- `src/acp.rs`
- `src/agent_loop.rs`
- `src/tools/mod.rs`
- `src/tools/fs.rs`

Read-only prompts may inspect real source files. Do not edit real agent Rust files to make health prompts pass — diagnose failures from logs and sandbox output.

### Current health-test focus

The next health run should specifically verify:
- Patch failures expose structured diagnostics and the model uses them to recover.
- `patch_file_tool` does not loop forever on identical failing `old_text`.
- `create_artifact_tool` writes exact user-provided content when an exact filename is supplied.
- Generated artifacts respect user constraints such as "short" or "under N lines".
- Final answers do not claim success unless a completed write/command tool result supports it.
- Read-only code questions search symbols and cite source-backed function/file references.

## Known Bugs (observed April 2026)

### Bug 7 — patch_file_tool target failures lacked actionable diagnostics (MEDIUM) ✅ Fixed
**Symptom:** The model saw only `Patch target not found in file`, repeatedly assumed
`old_text` was malformed, and retried near-identical patches without enough evidence.
**Fix:** `patch_file_tool` now returns structured diagnostics with occurrence counts,
line anchors, display-prefix detection, and newline/escape alternate counts. The agent loop
embeds those diagnostics in the model-facing tool-result envelope.

### Bug 6 — exact content artifact requests drifted during generation (HIGH) ✅ Fixed
**Symptom:** When the user provided exact markdown content plus a filename, `create_artifact_tool`
asked the model to regenerate the content and sometimes produced a longer, different file.
**Fix:** Explicit filename + content-looking instruction is now written directly. Content-only
instructions without a filename are rejected with a clear error.

### Bug 5 — Gemma `<|"|>` argument parsing decoded source escapes incorrectly (HIGH) ✅ Fixed
**Symptom:** Gemma tool args containing source string literals like `.join("\\n")` were decoded
into actual newlines before reaching `patch_file_tool`, so exact `old_text` could not match.
**Fix:** Gemma-delimited values preserve literal source escapes while regular quoted strings
still decode control escapes.

### Bug 4 — repeated identical failed tool calls wasted turns (MEDIUM) ✅ Fixed
**Symptom:** The model could call the same failing tool with the same arguments many times in
one turn sequence.
**Fix:** `run_agent_loop` blocks the third identical failed call and returns a structured failed
tool result telling the model to re-read context or choose a different approach.

### Bug 3 — read_file_tool char offset vs line number mismatch (HIGH) ✅ Fixed
**Symptom:** `search_code_tool` returns 1-based line numbers, but `read_file_tool` used character-based `offset`. Model passed line 422 as `offset=422` (char 422 ≈ file start), saw the same content repeatedly, hit 15-iteration max.
**Fix:** `read_file_tool` now takes `start_line` (1-based) and `limit` (lines, default 100). Output includes line numbers (`N: content`) matching search results. The `file_chunk_lines` helper in `tools/mod.rs` implements this.

### Bug 2 — Tool call injection from web content (HIGH) ✅ Fixed
**Symptom:** After `web_fetch_tool` returns a page that contains `<function=…>` XML, the parser fires on that content and executes the embedded examples as real tool calls.
**Fix:** `message_to_dict` in `main.py` escapes `<function=`, `<tool_call>`, `</tool_call>` sequences in tool results before inserting into conversation.

## Tests

100 total — all pass. Run: `cargo test`
