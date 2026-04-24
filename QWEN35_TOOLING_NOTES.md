# Qwen 3.5 Tooling and Prompt Notes

Target model:
- `Jackrong/MLX-Qwopus3.5-9B-v3-8bit`

This note collects the official Qwen guidance that is most relevant to tool use and prompt formatting.
It is intentionally not a migration plan.

## Official sources used

- [Qwen3 README](https://github.com/QwenLM/qwen3)
- [Qwen3 quickstart](https://github.com/QwenLM/Qwen3/blob/main/docs/source/getting_started/quickstart.md)
- [Qwen-Agent README](https://github.com/QwenLM/Qwen-Agent)
- [Qwen3.5 README](https://github.com/QwenLM/Qwen3.5/blob/main/README.md)

## Tool use

The official Qwen docs point developers toward Qwen-Agent for tool use and function calling.
The Qwen3 README says tool use can also be done through frameworks such as SGLang, vLLM, Transformers, llama.cpp, and Ollama.

Practical takeaways:
- Prefer a single normalized tool-call contract in the app layer.
- Keep tool schemas simple and explicit.
- Let the model emit structured tool calls when the backend supports it.
- If the framework supports parallel function calls, use them for independent work.

Qwen-Agent also documents native support for parallel function calls in its default tool-calling template.

## Prompt formatting

The Qwen3 quickstart documents two useful controls:

- `enable_thinking=False` disables thinking mode for the turn.
- `/think` and `/no_think` are soft instructions that can switch the model between thinking and non-thinking behavior in multi-turn conversations.

The quickstart also gives generation recommendations:

- Thinking mode: `temperature=0.6`, `top_p=0.95`, `top_k=20`, `min_p=0`
- Non-thinking mode: `temperature=0.7`, `top_p=0.8`, `top_k=20`, `min_p=0`

Practical takeaways:
- Keep the system prompt short and direct.
- Put the task in the user message plainly.
- Use thinking mode only when the task benefits from it.
- For tool-heavy agent loops, be explicit about whether the model should think or answer directly.
- Prefer concise instructions over large prompt essays.

## Recommended prompt shape

For this repo, the best practical shape is:

1. Short system instructions
2. Clear user task
3. Explicit tool guidance
4. Explicit final-answer format
5. Minimal extra prose

That keeps the prompt aligned with Qwen's thinking controls while leaving room for the model to manage tool use and planning.

## Notes for tool-call loops

- Qwen-Agent is the official Qwen-recommended path for agent/tool workflows.
- The docs emphasize function/tool use as a first-class capability.
- For long-running agent loops, keep the prompt and tool outputs compact enough that the model can keep track of phase boundaries.

## Useful links

- [Qwen3 README - Build with Qwen3 / Tool Use](https://github.com/QwenLM/Qwen3#build-with-qwen3)
- [Qwen3 quickstart - thinking mode and soft switch](https://github.com/QwenLM/Qwen3/blob/main/docs/source/getting_started/quickstart.md)
- [Qwen-Agent README - function calling and parallel calls](https://github.com/QwenLM/Qwen-Agent)
- [Qwen3.5 README](https://github.com/QwenLM/Qwen3.5/blob/main/README.md)
