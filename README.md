# Lucy

Lucy is a small local agent harness for macOS and Linux. It provides an interactive terminal chat UI over one OpenAI-compatible Chat Completions provider and one model-facing `cmd` tool, while retaining a normalized JSONL protocol for automation.

## Build and run

```sh
cargo build --release
./target/release/lucy
```

With no mode flag, Lucy starts the TUI when both stdin and stdout are terminals. If either stream is not a terminal, it automatically uses JSONL. `--jsonl` forces JSONL; `--tui` forces the TUI and fails clearly unless both stdio streams are terminals. `--session <id>` starts or resumes the selected session, and `--list-sessions` retains its JSONL metadata behavior.

The TUI has a borderless transcript and an input line with only top/bottom rules. User messages use a muted yellow background, and the input cursor blinks while input is available. Enter sends a message, ordinary input is ignored while a turn is running, Esc cancels the active turn, Ctrl-C exits, and Up/Down/PageUp/PageDown or the mouse wheel scroll the transcript. The transcript follows the newest message by default; manual scrolling pauses that follow mode until End or a new message is sent. Assistant text is rendered as provider deltas arrive; normalized tool calls/results, errors, completion, and resumed history are shown in the same order as the shared turn engine events.

A process handles one active turn at a time. TUI work runs in a worker so terminal rendering remains responsive. Esc stops a provider stream at the nearest safe point and kills a running command process group where possible. A canceled command is shown as an explicit bounded result; pending declared calls do not execute. A canceled turn emits `turn_interrupted` instead of `turn_end` and appends a valid interruption record to the session, including the phase, `user_cancelled` reason, safe partial assistant text, and safe tool observations.

In JSONL mode, Lucy reads LF-delimited message records from stdin and writes only normalized LF-delimited events to stdout; diagnostics and startup failures go to stderr.

Input messages have this shape:

```json
{"type":"message","text":"Inspect the project and summarize it."}
```

A successful turn emits normalized Lucy events. Provider response chunks are never forwarded. `--list-sessions` emits `session_metadata` records with `session_id`, `created_at`, `updated_at`, `first_message`, and `last_message` instead of a startup event:

```json
{"type":"session","session_id":"1770000000000-1234-0","resumed":false}
{"type":"assistant_delta","text":"I will inspect it. "}
{"type":"tool_call","id":"call-1","name":"cmd","arguments":"{\"command\":\"pwd\"}"}
{"type":"tool_result","id":"call-1","name":"cmd","result":{"command":"pwd","exit_code":0,"timed_out":false,"stdout":"/work\n","stderr":"","stdout_truncated":false,"stderr_truncated":false}}
{"type":"assistant_delta","text":"The project is ..."}
{"type":"turn_end"}
```

The input `text` must be a string; malformed records produce `error` records and do not terminate the process. Errors are normalized as `{"type":"error","message":"..."}` when a turn is active. Raw OpenAI/OpenRouter JSON, API keys, and provider chunk fields such as `choices` are not public events.

JSONL consumers should handle the normalized interruption event emitted by the shared engine. In the TUI, Esc emits the safe events already available and then one interruption event; it never emits `turn_end` for that turn:

```json
{"type":"turn_interrupted","reason":"user_cancelled","phase":"cmd"}
```

## Configuration and credentials

On a new run, Lucy creates `~/.lucy/config.toml` only when it is absent. It never overwrites an existing file. The generated file is intentionally minimal and editable:

```toml
system_prompt = "You can access computer resources. Use the provided tools to achieve the user's requirements. When needed, use cmd to read a relevant skill's SKILL.md."

[llm]
base_url = "https://openrouter.ai/api/v1"
model = ""
api_key_env = "OPENROUTER_API_KEY"
# Optional reasoning effort sent as the OpenAI Chat Completions "reasoning_effort"
# field, e.g. "low", "medium", "high". Omit or leave unset to send no effort.
# Use a value your provider and model support; an unsupported value fails at runtime.
# effort = "medium"
```

Set `model` before starting a session. Lucy does not guess a model. `base_url` is used as `<base_url>/chat/completions`; any OpenAI-compatible HTTP(S) endpoint can be configured. If `api_key_env` is omitted, runtime uses `OPENAI_API_KEY`.

An optional `[llm]` `effort` sets the OpenAI Chat Completions `reasoning_effort` request field for reasoning-capable models (for example `low`, `medium`, `high`). Omit it or leave it unset to send no effort. The value is sent verbatim, so use one your provider and model accept; an unsupported value is a runtime provider error, not a boot failure. An empty or whitespace-only `effort` fails boot with a configuration error. The resolved `effort` is persisted in the session snapshot and applies on resume.

The key is read only from the named environment variable. It is not written to config, session files, stdout, or diagnostics. Missing model and missing key errors are stable generic diagnostics and do not print the environment-variable name or secret. Keys containing JSON syntax/control characters, only decimal digits, or complete fixed protocol/storage literals are rejected before session output; this prevents redaction from corrupting JSON syntax, schema keys, or typed fields. TUI mode additionally rejects keys containing its fixed input/border characters. Newly created session metadata is also rejected if it contains the active key. Structured JSON tool arguments are recursively redacted before protocol and session persistence, including decoded Unicode-escaped strings and unknown object keys; required tool and result field names remain unchanged. Raw provider arguments remain the inputs used for local command execution. Malformed provider arguments are replaced with a valid empty-object placeholder in persisted/provider-facing history and are not executed as commands.

The credential guarantee covers direct child-process inheritance and Lucy-controlled serialized output or persistence. It does not provide full OS process isolation or prevent transformed side-channel exfiltration. A resumed file whose current key is already present is rejected, but changing credentials does not migrate or rewrite old-key session data; old-key rotation remains a residual limitation.

```sh
export OPENROUTER_API_KEY="..."
./target/release/lucy
```

Provider HTTP requests have a bounded 60-second timeout. An unreachable or stalled provider produces a normalized `error` event instead of hanging indefinitely. Provider response accumulation has conservative bounds on assistant text, tool-call count, and tool arguments. SSE lines are limited to 64 KiB, complete data events to 1 MiB, complete streams to 8 MiB, and data events to 1,024 lines; tool-call IDs/names and provider error text are bounded as well. A turn permits at most 32 provider tool rounds and 64 total `cmd` calls across those rounds. Exceeding either budget emits a normalized `error`; an over-budget response emits no further tool calls or provider request.

## Sessions

New sessions are stored as JSONL files at `~/.lucy/sessions/<session-id>.jsonl` and announce their ID in the first protocol event. On macOS/Linux, Lucy requests owner-only permissions (0700 for persisted directories and 0600 for persisted files). Resume a session with:

```sh
./target/release/lucy --session <session-id>
```

A missing session ID is a failure. List safe metadata as JSONL with:

```sh
./target/release/lucy --list-sessions
```

Metadata includes the ID, creation/update timestamps (Unix milliseconds), and bounded first/last message summaries. Each session stores the exact resolved boot system prompt plus the provider settings snapshot and all user, assistant, tool-call, and tool-result messages needed to continue. An interrupted turn appends a valid `record: "interruption"` JSONL record with its phase/reason and safe partial assistant/tool observations; incomplete provider tool fragments are never added to provider message history. A safe canceled `cmd` result observation can close a previously persisted assistant tool call when its ordinary message append failed. Resume replays the ordered history, including interruption records, and uses the saved snapshot without rereading current config, instruction, or skill files, even when the current config is malformed; mutable context changes apply to a new session. Provider reasoning metadata is recursively redacted and bounded before persistence or provider reuse; it remains private session state and is never emitted through the public protocol or rendered in the TUI. Lucy bootstraps a missing config before dispatching `--list-sessions`, but listing does not initialize a provider or validate current-config provider settings; malformed or credential-unsafe session files are skipped.

## Context and skills

At new-session boot, Lucy builds one deterministic system message in this order:

1. configured `system_prompt`;
2. full instruction text from global `~/.lucy/AGENTS.md` or `~/.lucy/CLAUDE.md`;
3. full instruction text from each directory from the Git root to the starting cwd, broad to specific. `AGENTS.md` wins over `CLAUDE.md` in one directory;
4. a sorted catalog of standard skills.

Only these Agent Skills locations are discovered:

- global `~/.agents/skills/<name>/SKILL.md`;
- `.agents/skills/<name>/SKILL.md` in each project ancestor from Git root to cwd.

A skill needs `name` and `description` frontmatter. Invalid or incomplete files are skipped. More-specific project entries override global or broader project entries with the same name. The boot prompt includes only each skill's name, description, and path, not its full contents. The model can use `cmd` to read a relevant `SKILL.md` when needed.

## `cmd` behavior

The only model-facing tool is `cmd`, whose arguments contain only a command string:

```json
{"command":"find . -maxdepth 2 -type f"}
```

Lucy executes `/bin/sh -lc <command>` from the session's starting cwd, inherits the Lucy environment except that the configured provider API-key environment variable (`llm.api_key_env`) is removed before the shell starts, disconnects stdin, and does not pass the key through another shell mechanism or mutate cwd between calls. Each invocation has a 10-minute timeout. stdout and stderr are captured independently up to 64 KiB, with `stdout_truncated` and `stderr_truncated` flags. A flag is also set when capture shutdown reaches its bounded 100 ms grace period before EOF, so detached-reader output is marked incomplete rather than reported as complete. A non-zero exit code is a normal tool result. On macOS/Linux, each shell gets its own process group and Lucy attempts to kill that group after timeout or shell exit. A descendant that deliberately escapes the process group/session can continue running outside Lucy, and output still held open by it may not be captured after that grace period. Full daemon escape containment is not provided. The shell cannot inherit the configured key directly, but Lucy does not provide OS-level isolation from parent-process inspection. Interactive commands, daemons, background-process management, approval, sandboxing, and persistent shell state are not supported.

## v1 non-goals

Lucy v1 deliberately does not include an HTTP server, MCP, read/write/edit/file tools, sandbox, authentication server, approval UI, multiple provider abstraction, compaction, concurrent sessions in one process, or interactive/background process support. It is a trusted local macOS/Linux harness, not a remote service or full coding-agent product.

## Checks

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features --locked -- -D warnings
```

The tests use local command execution and a local mock HTTP server; they do not require live credentials or network access.
