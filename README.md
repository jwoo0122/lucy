# Lucy

## Project purpose

Lucy is a lightweight local coding-agent harness for macOS and Linux. It connects one OpenAI-compatible Chat Completions provider and a model-facing `cmd` tool, with both an interactive TUI and a JSONL interface for automation powered by the same turn engine.

## Installation

Prebuilt releases are available for Apple Silicon macOS, Intel macOS, and x86_64 Linux. The recommended installation method is Homebrew:

```sh
brew install jwoo0122/tap/lucy
```

The `lucy-cli` crate is also published on crates.io for Rust users:

```sh
cargo install lucy-cli
```

Prebuilt archives are available from the [GitHub Releases](https://github.com/jwoo0122/lucy/releases) page. After extracting the archive, place the `lucy` executable on your `PATH`.

On first run, Lucy creates `~/.lucy/config.toml`. Set `llm.model` and the environment variable that holds the API key. OpenRouter is the default provider, but any OpenAI-compatible endpoint can be configured.

```toml
[llm]
base_url = "https://openrouter.ai/api/v1"
model = "your-model"
api_key_env = "OPENROUTER_API_KEY"
```

```sh
export OPENROUTER_API_KEY="..."
```

## Usage

Run Lucy in a terminal to start the TUI. Use the release binary path when building from source:

```sh
lucy
# Or: ./target/release/lucy
```

Lucy automatically uses JSONL mode when either standard input or output is not a terminal. Use `--tui` or `--jsonl` to choose a mode explicitly.

```sh
printf '%s\n' '{"type":"message","text":"Inspect the project."}' | lucy --jsonl
lucy --session <session-id>
lucy --list-sessions
```

In the TUI, press Enter to send, Shift/Alt+Enter to insert a line break, and Esc to cancel the active turn. Enter `/<name> [args]` to attach the saved `SKILL.md` snapshot for that skill to the next model request.

## Features

- **TUI and JSONL:** Supports terminal chat and line-delimited JSON automation.
- **Streaming activity:** Shows model output, reasoning wait states, tool calls/results, and cancellation status in the TUI.
- **Completion notifications:** When a TUI turn becomes idle, Lucy sends a terminal-native OSC 777 desktop notification for completion, cancellation, or error when the terminal supports it; JSONL output is unchanged.
- **Safe local command execution:** Exposes only `cmd` to the model and runs shell commands from the session's starting directory with time and output limits.
- **Persistent sessions:** Stores conversation history, provider settings, boot context, and skill snapshots as JSONL in `~/.lucy/sessions/` and supports resuming them.
- **Context and skills:** Collects `AGENTS.md`/`CLAUDE.md` instructions and Agent Skills for new sessions. The model sees only skill metadata; explicit slash-prefixed skill-name invocations use the saved snapshot.
- **Automatic context compaction:** At 95% estimated context usage, safely summarizes older complete turns with the configured model, retains recent context, and resumes the active turn without rewriting session history.
- **Credential protection:** Reads API keys only from environment variables and prevents them from being written to configuration, sessions, the public protocol, or diagnostics.
