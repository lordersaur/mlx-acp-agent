# mlx-acp-agent

A local coding agent written in Rust, speaking the [Agent Client Protocol (ACP)](https://zed.dev/acp) over stdio. Plugs into the Zed editor's agent panel and uses a local MLX inference server for all model calls.

**Requires:** [python-mlx-sv](https://github.com/lordersaur/python-mlx-sv) running locally

## Setup

**1. Install Rust**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**2. Clone and build**
```bash
git clone https://github.com/lordersaur/mlx-acp-agent
cd mlx-acp-agent
cargo build --release
```

The binary is at `target/release/rust-agent`.

**3. Start the MLX server**

Follow the setup instructions in [python-mlx-sv](https://github.com/lordersaur/python-mlx-sv), then:
```bash
source ~/mlx-env/bin/activate
cd ~/python-mlx-sv && uvicorn main:app
```

**4. Configure Zed**

Add the agent to your Zed `settings.json`:
```json
{
  "agent": {
    "profiles": {
      "mlx": {
        "name": "MLX Local Agent",
        "tools": {},
        "enable_all_context_servers": false
      }
    }
  }
}
```

Then register the binary as an external agent pointing to `target/release/rust-agent`.

## Configuration

| Environment variable | Default | Description |
|---|---|---|
| `MLX_URL` | `http://127.0.0.1:8000/v1/chat/completions` | MLX server endpoint |
| `MLX_MODEL` | `mlx-community/gemma-4-e4b-it-OptiQ-4bit` | Model name reported to the server |

## Modes

Switch modes from the Zed agent panel dropdown:

| Mode | Description |
|---|---|
| **Ask** | Read-only — answers questions and inspects code |
| **Edit** | File changes only — read, patch, create |
| **Agent** | Full mode — search, web, shell, edits, validation |
| **Fast** | Agent mode with thinking disabled — faster responses |

## Tools (15)

`read_file`, `list_dir`, `search_code`, `web_search`, `web_fetch`, `run_command`,
`start_command_session`, `list_command_sessions`, `read_command_session`,
`write_command_session`, `terminate_command_session`, `patch_file`, `delete_path`,
`edit_file`, `create_artifact`

## Running tests

```bash
cargo test
```
