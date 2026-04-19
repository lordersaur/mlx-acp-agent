# Agent Handoff

## Current Status

- Repo: `/Users/daxel/mlx-acp-agent`
- Branch: `agent-health-disposable`
- Current baseline commit: `f183032 Prepare agent health sandbox tests`
- Git status after that commit was clean.
- This `agent.md` file may be the only uncommitted change if it was just
  created for handoff.
- `target/` is no longer tracked by Git and is already ignored.

## Project Summary

This repo is a Rust local coding agent that speaks ACP JSON-RPC over stdio for
Zed. It uses a local MLX-compatible OpenAI-style chat completions server.

Important files:

- `src/acp.rs` - ACP server, session handling, Zed updates, tool panel output.
- `src/agent_loop.rs` - model/tool loop, `SYSTEM_PROMPT`, response guards.
- `src/tools/mod.rs` - built-in tool registry and tool schemas.
- `src/tools/fs.rs` - filesystem helpers for read/list/search/patch/create.
- `src/tools/command.rs` - command execution and command sessions.
- `src/session_store.rs` - persisted session state and turn records.
- `agent-health-test-prompts.md` - current safe health-test suite.
- `portfolio-project-agent-test.md` - later full portfolio project test.

## What We Are Doing

We are preparing safer real-world health tests for the local Rust ACP coding
agent before attempting the senior developer portfolio project test.

The immediate goal is to run `agent-health-test-prompts.md` through Zed/ACP and
verify the agent can:

- inspect real source without modifying it,
- modify only scratch/sandbox files when asked to change code,
- show real tool success/error output,
- avoid phantom success claims,
- recover from wrong paths,
- run validation after sandbox edits,
- keep `src/**/*.rs` agent implementation files untouched during health tests.

## Important Safety Rule

Health prompts that mutate files must only touch:

- `HEALTH_*`
- `HEALTH_SANDBOX/**`

They must not modify real agent implementation files such as:

- `src/acp.rs`
- `src/agent_loop.rs`
- `src/tools/mod.rs`
- `src/tools/fs.rs`

Read-only prompts may inspect real source files.

Do not edit real agent Rust files just to make health prompts pass. Health
prompt failures should first be diagnosed from logs and sandbox output.

## Test Files

- `agent-health-test-prompts.md`
  - Main health suite.
  - Now uses `HEALTH_SANDBOX/fixture-crate` for source-editing prompts.
  - Preflight creates the fixture crate and runs validation.

- `portfolio-project-agent-test.md`
  - Later end-to-end project test.
  - Do not run until the health suite is passing.

- `HEALTH_CONTRIBUTING.md`
  - Scratch doc created by an earlier health prompt.

## Recent Meaningful Changes

- Tool completion panels in `src/acp.rs` now show raw tool output/error text.
- `src/tools/fs.rs` now reports real filesystem errors for read/list failures.
- `src/agent_loop.rs` has guards for malformed raw tool-call marker output and
  stricter test-pass claims.
- `agent-health-test-prompts.md` was changed so risky edits target the fixture
  crate instead of real Rust agent files.
- `target/` build artifacts were removed from Git tracking.

External context:

- The local MLX server repo is `/Users/daxel/python-mlx-sv`.
- `main.py` there was recently adjusted to recover Gemma native tool calls that
  have a `<tool_call|>` terminator but omit the final `}`.
- That external repo may still have its own uncommitted changes. Do not assume
  it is clean.

## How To Run The System

Start the MLX server in a separate terminal:

```bash
source ~/mlx-env/bin/activate
cd ~/python-mlx-sv
uvicorn main:app
```

Useful logs from that server include:

- `user_prompt=...`
- `raw_output=...`
- `extract_tool_calls matched=...`
- `finish_reason=...`

Run Rust validation from this repo:

```bash
cargo fmt --check
cargo test
```

Run fixture validation after health preflight:

```bash
cargo test --manifest-path HEALTH_SANDBOX/fixture-crate/Cargo.toml
```

## Recommended Next Step

Run the preflight block from `agent-health-test-prompts.md`, then run prompts
1-17 through Zed/ACP.

After the run, collect:

```bash
git log -1 --oneline
git status --short
```

For any failed prompt, save:

- prompt number,
- expected behavior,
- actual behavior,
- relevant Zed tool panel output,
- relevant `mlxsv` logs with `user_prompt`, `raw_output`,
  `extract_tool_calls`, and `finish_reason`.

## Report Format For Next Session

```text
Baseline commit:
f183032 Prepare agent health sandbox tests

Failed prompt:
#N ...

Expected:
...

Actual:
...

git status --short:
...

Relevant mlxsv logs:
...
```

## New Session Checklist

When starting a new assistant session for this work:

1. Read this `agent.md` first.
2. Check:

   ```bash
   git log -1 --oneline
   git status --short
   ```

3. If health tests were already run, inspect only the changed files and logs the
   user provides.
4. Treat changes under `HEALTH_*` and `HEALTH_SANDBOX/**` as expected health
   artifacts.
5. Treat changes under real `src/**/*.rs` after a health run as a likely failure
   unless the user explicitly requested real implementation work.
6. Do not delete `CLAUDE.md` unless the user explicitly asks for cleanup.

## Notes On CLAUDE.md

`CLAUDE.md` still exists but appears partially stale and duplicated. Keep it for
now unless explicitly cleaning old docs. Use this `agent.md` as the current
handoff for the health-test work.
