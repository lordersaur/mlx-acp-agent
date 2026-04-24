# Qwen 3.5 Migration Plan

Target model:
- `Jackrong/MLX-Qwopus3.5-9B-v3-8bit`

Scope:
- `mlx-acp-agent`
- `python-mlx-sv`

This plan is based on the Qwen 3.5 tool-use and prompt-format guidance captured in
[QWEN35_TOOLING_NOTES.md](QWEN35_TOOLING_NOTES.md).

## Goals

1. Switch both repos from Gemma-specific behavior to Qwen 3.5 behavior.
2. Keep tool calls reliable and structured.
3. Keep prompts short and explicit.
4. Preserve long-running agent quality without hardcoding one-off fixes.

## What the Qwen docs imply

From the official Qwen docs:

- Qwen recommends Qwen-Agent for tool use and function-calling workflows.
- Qwen3 supports function calling and parallel function calls.
- Qwen3 prompt formatting supports `enable_thinking=False` for non-thinking turns.
- Qwen3 also supports `/think` and `/no_think` as soft switches in multi-turn conversations.
- Qwen3 quickstart recommends different sampling settings for thinking vs non-thinking mode.

Practical consequence:

- Keep the agent prompt concise.
- Make thinking mode explicit.
- Prefer native structured tool calls where the backend supports them.
- Use parallel tool calls for independent work.

## Repo 1: `python-mlx-sv`

This repo is the first priority because it sits directly in the model I/O path.

### 1. Remove Gemma-specific parsing

Current Gemma-specific symbols to replace or generalize:

- `_is_gemma_native_model(...)`
- `_scan_gemma_calls(...)`
- `_extract_thought(...)`
- `_apply_gemma_thinking_marker(...)`
- `_prefill_empty_thought_channel(...)`
- `_dedup_cap_calls(...)`
- the Gemma-native assistant/tool message attachment path

Planned Qwen-side replacement:

- a Qwen-specific dialect detector
- a qwen tool-call parser only if the model does not emit native structured tool calls cleanly
- a qwen thought extractor only if the backend still emits explicit thought markers

Preferred direction:

- if `Jackrong/MLX-Qwopus3.5-9B-v3-8bit` emits native structured tool calls reliably, prefer that path and delete the Gemma text parser.
- if it emits `reasoning_content`, preserve it in message history instead of stripping it away.

### 2. Update chat-template handling

The current server logic does three Gemma-specific things:

- inserts a Gemma thinking marker
- pre-fills an empty thought channel
- trims Gemma thinking markers from output

For Qwen, replace that with:

- explicit `enable_thinking` control based on the turn mode
- a qwen-safe non-thinking mode for direct answers and tool-loop phases
- no Gemma-specific tag injection unless the Qwen model actually proves it needs one

### 3. Rework tool-call parsing around Qwen output

The migration should validate the actual raw output from the Qwen model first.

Then choose one of these paths:

- Native structured `tool_calls`
  - simplest path
  - preferred if the model and backend already emit them
- Text parsing fallback
  - only if the model emits XML-like tool blocks or reasoning text that needs cleanup

The parser should be updated so that:

- tool-call extraction is not tied to Gemma tags
- reasoning text is preserved or separated in the format Qwen actually emits
- the server response stays normalized for the Rust agent

### 4. Adjust sampling defaults if needed

Use the Qwen quickstart defaults as a baseline:

- thinking mode: `temperature=0.6`, `top_p=0.95`, `top_k=20`, `min_p=0`
- non-thinking mode: `temperature=0.7`, `top_p=0.8`, `top_k=20`, `min_p=0`

If tool loops get noisy, tune only after checking the raw model output.

### 5. Update tests

Add or rewrite tests around:

- native Qwen tool calls
- Qwen thinking/non-thinking mode switches
- tool-call loops with reasoning preserved in history
- malformed or partial tool-call output

## Repo 2: `mlx-acp-agent`

This repo should be updated to consume the normalized output from the MLX server without Gemma assumptions.

### 1. Remove Gemma-only parsing assumptions

Current Gemma-specific areas to review:

- `src/model_parser.rs`
- `src/mlx_client.rs`
- `src/acp.rs`
- `src/config.rs`

Planned changes:

- make thought stripping dialect-aware instead of Gemma-only
- preserve Qwen reasoning if the server returns it in a dedicated field
- keep user-visible answers free of raw control tokens
- stop assuming Gemma tag pairs are the only valid thought markers

### 2. Update model configuration

Change the default model name from the Gemma branch to the Qwen branch/model.

Then make sure the agent loop can still:

- request tools
- receive structured tool calls
- stream thoughts separately if the backend emits them
- keep tool results normalized

### 3. Keep the agent loop generic

The agent loop should stay focused on orchestration:

- prompt assembly
- model call
- tool execution
- tool results fed back
- repeat until final answer

Avoid encoding qwen-specific rules in the loop if the Python server can normalize them first.

### 4. Tune prompt formatting for Qwen

Use the Qwen docs to simplify the prompt stack:

- short system prompt
- short task wrapper
- explicit thinking / non-thinking behavior
- explicit tool-use guidance
- minimal extra prose

For Qwen 3.5 specifically, this should be narrower than the older Gemma prompt stack.

### 5. Update tests

Add tests for:

- Qwen-style tool-call outputs
- preserved reasoning content when returned by the server
- plain answers without tool calls
- mixed thought + tool-call turns

## Suggested implementation order

1. Probe the Qwen model once to confirm its raw output format.
2. Update `python-mlx-sv` parsing and chat-template behavior.
3. Update `mlx-acp-agent` to consume the new normalized output.
4. Update prompts and defaults.
5. Run the broad agent workflow tests again.

## Success criteria

The migration is done when:

- Qwen tool calls are extracted reliably.
- Tool results round-trip cleanly through the conversation.
- Reasoning is preserved or filtered in the right place.
- The agent no longer depends on Gemma-specific tags or parsing.
- Broad audit tasks complete with a final plan instead of looping or stopping early.

