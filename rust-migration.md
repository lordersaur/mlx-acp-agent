# Python to Rust Migration

## Goal

Move this agent from the current Python reference runtime to a Rust primary
runtime for:

- single-binary deployment
- lower memory use
- fewer environment / dependency issues
- more reliable networking and process handling
- cleaner long-term architecture

This migration is **not** expected to materially improve model latency. Most
end-to-end latency still comes from:

- MLX HTTP roundtrips
- model generation time
- multi-turn tool loops
- subprocess / network I/O

So the migration should optimize for deployability and runtime quality, not for
"the model will suddenly feel fast".

## Recommendation

Do **not** finish every remaining feature in Python first.

Recommended sequencing:

1. Use the current Python agent as the behavior spec for Phases 0-4.
2. Port the deterministic core to Rust now.
3. Implement new structural work in Rust:
   - Phase 5 terminal session upgrade
   - remaining Phase 6 refinement carryover
   - Phase 7 subagents

This avoids building Phase 5 and 7 twice.

## Current Source of Truth

The Python reference runtime has been moved to `/Users/daxel/mlx-acp-agent-python/`.
The Rust agent is now the primary runtime at `/Users/daxel/mlx-acp-agent/`.

Python files (archived reference):

- `agent.py`: ACP protocol handler and session control
- `agent_loop.py`: tool-calling loop
- `agent_tools.py`: tool facade and progress events
- `tools.py`: filesystem and terminal tools
- `web.py`: web fetch/search tools
- `model.py`: response parsing and tool-call normalization
- `mlx_client.py`: MLX HTTP client
- `sessions.py`: session store

## Migration Principles

- Preserve external behavior first, improve internals second.
- Keep MLX unchanged; Rust should talk to the same OpenAI-compatible endpoint.
- Port deterministic logic before ACP integration.
- Keep the Python runtime runnable until Rust reaches feature parity.
- Prefer black-box tests and fixture-driven parser tests over re-deriving behavior from memory.

## Target Rust Architecture

Suggested layout:

```text
mlx-acp-agent/
  Cargo.toml
  src/
    main.rs
    lib.rs
    config.rs
    mlx_client.rs
    model_parser.rs
    session_store.rs
    tools/
      mod.rs
      fs.rs
      command.rs
      web.rs
    acp.rs
```

Core crates:

- `tokio`
- `reqwest` with `rustls-tls`
- `serde`
- `serde_json`
- `regex`
- `tracing`
- `anyhow`
- `uuid`
- `portable-pty`
- `scraper`

## File Mapping

### Phase 1: deterministic core

- `mlx_client.py` -> `src/mlx_client.rs`
- `model.py` -> `src/model_parser.rs`
- `sessions.py` -> `src/session_store.rs`
- `agent_loop.py` -> `src/agent_loop.rs`

### Phase 2: tools

- `tools.py` -> `src/tools/fs.rs` and `src/tools/command.rs`
- `web.py` -> `src/tools/web.rs`
- `agent_tools.py` -> Rust tool registry / facade

### Phase 3: protocol and cutover

- `agent.py` -> `src/acp.rs`
- Python ACP shim only if a full Rust ACP path is awkward initially

## Milestones

### Milestone 0: Characterize Current Behavior

Capture fixtures for:

- standard `<tool_call>` JSON
- Qwen native `call:name{args}`
- malformed tool call retries
- `<thinking>` and `<|channel>thought` handling
- representative tool progress events

Deliverables:

- parser fixtures
- sample MLX request/response logs
- expected tool result formatting

### Milestone 1: Rust Core Scaffold

Deliverables:

- Rust crate compiles
- config loading matches Python defaults
- MLX client can call the existing server
- parser module exists with unit tests for current tool-call formats

Status:

- started in this repo

### Milestone 2: Parser and MLX Parity

Deliverables:

- Rust parser handles:
  - standard tool-call blocks
  - Qwen native tool-call format
  - thought extraction
  - residual text normalization
- Rust MLX client can make the same request shape as Python

Exit check:

- Rust parser passes fixture tests from Milestone 0

### Milestone 3: Tool Loop Parity

Deliverables:

- Rust `run_agent_loop` equivalent
- same retry behavior for malformed tool calls
- same conversation formatting for tool schemas and tool results

Exit check:

- Rust loop can complete a simple inspect task against the current Python tool runtime or Rust mock tools

### Milestone 4: Session and Tool Runtime

Deliverables:

- session store parity
- filesystem tools
- command tools
- web tools

Exit check:

- Rust runtime handles Phases 0-4 feature set

### Milestone 5: ACP Integration and Cutover

Options:

1. direct Rust ACP implementation
2. thin Python ACP shim that forwards to Rust core

Recommendation:

- choose the thinner path first
- only keep the Python shim if ACP ergonomics in Rust are materially slower to ship

### Milestone 6: New Work Only in Rust

After parity:

- implement Phase 5 in Rust
- carry over remaining Phase 6 tuning in Rust
- implement Phase 7 in Rust
- deprecate Python runtime once stable

## Immediate Next Slice

Start with the deterministic core:

1. scaffold the Rust crate
2. port config defaults
3. port the MLX client
4. port the response parser with tests

That gives a useful foundation without touching ACP yet.

## Cutover Criteria

Switch the primary runtime to Rust when:

- parser behavior matches captured fixtures
- Rust can complete representative Phase 0-4 tasks against the MLX server
- terminal and web tool behavior are stable
- the ACP bridge is reliable enough for editor use
- the Python runtime is no longer the easier path for new features
