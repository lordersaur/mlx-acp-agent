# Gemma 4 Integration Plan

Model: `mlx-community/gemma-4-e4b-it-4bit`  
Goal: make the agent work correctly with Gemma 4 the same way it works with Qwen3.5.

---

## Confirmed Gemma 4 Formats (from Google docs + HF model card)

| Concern | Qwen3.5 | Gemma 4 |
|---|---|---|
| Tool calls | `<function=name>…</function>` XML | `<\|tool_call>call:name{args}<tool_call\|>` native tokens |
| Tool args (strings) | quoted normally | wrapped in `<\|"\|>` delimiters |
| Thinking open | `<think>` (pre-filled in prompt) | `<\|channel>thought\n` |
| Thinking close | `</think>` | `<channel\|>` |
| Turn tags | `<\|im_start\|>role` / `<\|im_end\|>` | `<\|turn>role` / `<turn\|>` |
| Tool result role | `user` + `<tool_response>` wrapper | `tool` natively |
| Fast mode | `/no_think` prefix → `enable_thinking=False` | omit `<\|think\|>` from system prompt |
| `apply_chat_template tools=` | yes | yes |
| `enable_thinking` kwarg | yes | no — will throw, must be excluded |

**Good news:** `model_parser.rs` already handles `<|channel>thought…<channel|>` blocks — no Rust changes needed for thinking extraction.

---

## Status

| Step | Description | Status |
|------|-------------|--------|
| 1 | Model family detection helper in `main.py` | 🔲 |
| 2 | `apply_chat_template` — gate `enable_thinking` on Qwen only | 🔲 |
| 3 | `message_to_dict` — use native `tool` role for Gemma | 🔲 |
| 4 | Tool call extraction — add `gemma_native` stage for `<\|tool_call>` format | 🔲 |
| 5 | `_normalize_gemma_args` — parse `{key:<\|"\|>val<\|"\|>}` into dict | 🔲 |
| 6 | `clean_output` — strip `<\|channel>thought…<channel\|>` blocks | 🔲 |
| 7 | Fast mode — omit `<\|think\|>` from system prompt for Gemma fast mode | 🔲 |
| 8 | `stream_think_chunk` — handle `<\|channel>thought` / `<channel\|>` tags | 🔲 |
| 9 | `extract_post_think` — handle `<channel\|>` end tag | 🔲 |
| 10 | Smoke test all prompts | 🔲 |
| 11 | Update CLAUDE.md quirks section | 🔲 |

---

## Files Changed

| File | Steps |
|------|-------|
| `~/python-mlx-sv/main.py` | 1–7 |
| `src/mlx_client.rs` | 8–9 |
| `src/acp.rs` | 7 (fast mode prefix) |
| `CLAUDE.md` | 11 |

---

## Step Detail

### Step 1 — Model family detection (`main.py`)
After `resolve_model_name()`, set a module-level constant used in all branches:
```python
_ACTIVE_MODEL = resolve_model_name()
MODEL_FAMILY = "gemma" if "gemma" in _ACTIVE_MODEL.lower() else "qwen"
```

### Step 2 — `apply_chat_template` (`main.py`)
`enable_thinking` is Qwen-only. Gemma's tokenizer will throw if it receives it.
```python
if MODEL_FAMILY == "qwen":
    template_kwargs["enable_thinking"] = not fast_mode
# Gemma: thinking controlled by <|think|> in system prompt (step 7), not a kwarg
```
Both model families accept `tools=` — no change needed there.

### Step 3 — `message_to_dict` (`main.py`)
Qwen needs `role: "tool"` → `role: "user"` with `<tool_response>` wrapper.
Gemma accepts `role: "tool"` natively — pass it through as-is.
```python
if msg["role"] == "tool":
    if MODEL_FAMILY == "qwen":
        # existing wrapper logic
    else:
        return msg  # Gemma handles tool role natively
```

### Step 4 — Tool call extraction (`main.py`)
Add `gemma_native` as the first extraction stage. Gemma emits:
```
<|tool_call>call:function_name{arg1:value1,arg2:<|"|>string<|"|>}<tool_call|>
```
Parse with a regex, then call `_normalize_gemma_args` (step 5) on the arg body.
Fall through to existing Qwen stages if no match (allows future multi-model sessions).

### Step 5 — `_normalize_gemma_args` (`main.py`)
Gemma wraps string values in `<|"|>…<|"|>`. Convert to a proper Python dict:
```python
def _normalize_gemma_args(body: str) -> dict:
    # replace <|"|>value<|"|> with "value"
    # split on commas outside string values
    # return {key: value} dict
```

### Step 6 — `clean_output` (`main.py`)
Add Gemma channel block stripping:
```python
def clean_output(text: str) -> str:
    for token in ["<|im_end|>", "<|im_start|>", "<turn|>", "<|turn>"]:
        text = text.replace(token, "")
    text = re.sub(r"<think>.*?</think>", "", text, flags=re.DOTALL)
    text = re.sub(r"(?s)<\|channel>thought\n.*?<channel\|>", "", text)
    return text.strip()
```

### Step 7 — Fast mode (`main.py` + `src/acp.rs`)
Qwen fast mode: `acp.rs` prepends `/no_think` to the system message; `main.py` detects it and sets `enable_thinking=False`.
Gemma fast mode: inject `<|think|>` into system prompt to enable thinking (default off for E4B), or omit it to disable. For fast mode, simply don't inject `<|think|>`.
- `acp.rs`: keep `/no_think` prefix for Qwen; add a `[FAST]` marker that `main.py` can detect model-agnostically.
- `main.py`: if `MODEL_FAMILY == "gemma"` and fast mode, don't append `<|think|>` to system.

### Step 8 — `stream_think_chunk` (`src/mlx_client.rs`)
Add Gemma channel tag detection alongside existing Qwen `<think>` handling.
Open tag: `<|channel>thought\n`
Close tag: `<channel|>`
The state machine already supports pre-filled thinking (close before open) — same pattern applies.

### Step 9 — `extract_post_think` (`src/mlx_client.rs`)
Add `<channel|>` as a known end tag:
```rust
fn extract_post_think(text: &str) -> String {
    for tag in &["</think>", "</thinking>", "<channel|>"] {
        if let Some(idx) = text.find(tag) {
            return text[idx + tag.len()..].trim().to_owned();
        }
    }
    text.trim().to_owned()
}
```

### Step 10 — Smoke tests
Run after steps 2–3 (basic completion), then after step 4 (tool calls), then full suite:
1. `What files are in the src/ directory?` — list_dir_tool
2. `Read src/config.rs and tell me what it does.` — read_file_tool
3. `Where is extract_post_think defined?` — search + read
4. `The tool output looks weird. Fix it.` — ambiguity, should ask not act

### Step 11 — CLAUDE.md
Replace "Qwen3.5 tool calling quirks" with a model-aware section covering both families.

---

## Fallback

Revert `DEFAULT_MODEL_NAME` in `main.py` to `mlx-community/Qwen3.5-9B-OptiQ-4bit`. Both models stay cached.
