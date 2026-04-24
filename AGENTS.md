Hello there! I am the magnificent Gemma 4, ready to assist with any coding challenge!
# mlx-acp-agent

Rust ACP coding agent for Zed.

## Current State

- The agent loop is intentionally minimal:
  - prompt assembly
  - model call
  - tool execution
  - tool results fed back
  - repeat until final answer
- The loop is in-memory only right now.
- Session reload persistence was removed on purpose.
- Tool descriptions are intentionally short and direct, but discovery tools may now include small metadata when that helps the model plan.
- `AGENTS.md` is the repo guidance file now. `CLAUDE.md` was removed.
- Broad audit workflows now rely on `list_dir_tool` plus `read_file_tool` rather than a source-tree dump tool.
- The loop tracks coverage locally and may inject a short past-tense summary of the previous reasoning/action between iterations.

## Important Files

- [`src/agent_loop.rs`](src/agent_loop.rs): core loop, model/tool traits, loop options.
- [`src/acp.rs`](src/acp.rs): ACP JSON-RPC server, session wiring, Zed UI updates.
- [`src/session_store.rs`](src/session_store.rs): in-memory session state and command session tracking.
- [`src/tools/mod.rs`](src/tools/mod.rs): built-in tool registry and schemas.
- [`src/tools/fs.rs`](src/tools/fs.rs): file read/search/edit tools.
- [`src/tools/command.rs`](src/tools/command.rs): shell and command-session tools.
- [`src/tools/web.rs`](src/tools/web.rs): web search and fetch tools.

## Working Rules

- Prefer the public agent-loop pattern over custom contract or evidence phases.
- Keep prompts and tool descriptions minimal, but make discovery/read handoffs explicit.
- Do not reintroduce disk-backed session persistence unless explicitly asked.
- Avoid hardcoded prompt fixes for single runs or single failures.
- For code questions, search for exact symbols and follow the call chain from source.
- For broad audits, use `list_dir_tool` with metadata to discover files, then `read_file_tool` for the relevant content.
- For direct single-file requests, read the named file directly unless the path is ambiguous or missing.
- Coverage completion is tracked in the loop; it should not be treated as a separate user-facing phase.
- If you need to summarize prior reasoning between iterations, keep it short and past tense.

## Runtime Notes

- The ACP server speaks JSON-RPC over stdio to the editor.
- `session/prompt` starts the model loop.
- `session/cancel` aborts the current task.
- `session/new` and `session/load` create or restore in-memory session state only.
- Tool progress and thought streaming are UI notifications, not agent logic.

## Testing

- `cargo test`
- `cargo check`
