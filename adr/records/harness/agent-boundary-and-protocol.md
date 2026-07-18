---
id: harness.agent-boundary-and-protocol
status: accepted
scope: harness
decision_type: boundary
applies_to:
  - "Cargo.toml"
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy is a local macOS/Linux harness with a default TUI, an automatic/explicit JSONL mode, one OpenAI-compatible provider, `cmd`, and a bounded `spawn_subagent` model tool.
constrains: []
depends_on: []
supersedes: []
superseded_by: []
last_reviewed: "2026-07-19"
---

# Local interactive and JSONL harness boundary

## Decision question

What public boundary and capability surface does the Lucy harness expose to interactive users and machine clients?

## Current decision

Lucy MUST run as a local macOS/Linux process and MUST retain its newline-delimited JSON machine protocol. When both standard input and standard output are terminals, an invocation without a mode flag MUST start the TUI. When either stream is not a terminal, the invocation MUST use JSONL automatically. `--jsonl` MUST force JSONL and `--tui` MUST force the interactive frontend; the latter requires a usable terminal. The TUI is a frontend over the same normalized event and turn engine, not a new provider or tool boundary. Its slash picker MUST combine discovered skill names with Lucy-owned `/settings` and `/exit` commands without persisting or expanding those commands as skills. `/settings` MUST ignore trailing arguments and open the idle-only settings menu; `/exit` MUST terminate an idle TUI session. The TUI MUST render each background worker as a compact one-line entry in a list between the message input and the bottom status line, showing task id, model, effort, and a short task preview. When the input has focus, Down from the last visual input row MUST move focus into that list when it is non-empty; Up from the first list row or Esc MUST return focus to the input. While a list entry is focused, the TUI MUST show that worker's live message stream in a bordered overlay immediately above the input at the same slot used by the slash skill picker. Worker stream observation is a TUI-internal frontend signal: it MUST NOT add public JSONL events. A `spawn_subagent` transcript card MUST settle as completed when creation succeeds (queued acknowledgement with a task id), without waiting for worker exit; `check_subagent` MUST use a compact one-line tool card comparable to `cmd` and `spawn_subagent`. After an active TUI turn reaches the worker-finished boundary and `busy` is released, the TUI MUST emit a terminal-native OSC 777 desktop notification for normal completion, interruption, or error; terminals without support may ignore this frontend signal. It MUST NOT be emitted in JSONL mode, and a notification write failure MUST NOT change turn behavior.

Lucy MUST NOT be a network service in v1. The harness MUST expose `cmd`, `spawn_subagent`, `check_subagent`, `wait_subagent`, `send_subagent`, and `cancel_subagent` as its only model tools; it MUST NOT provide built-in `read`, `write`, `edit`, or other file-operation tools. `spawn_subagent` accepts a task and optional model and reasoning-effort overrides. It creates an isolated, one-level worker with the session's immutable boot context and cwd but no parent conversation or tool output; omitted overrides inherit the session's provider settings. A worker can use `cmd` but MUST NOT receive `spawn_subagent`. Up to four workers run concurrently across the process. `spawn_subagent` returns a task ID and queued status immediately rather than waiting for the worker. The harness queues completion messages and automatically starts a follow-up main-agent turn to deliver each result; the main agent may also inspect a task through `check_subagent`. User messages accepted while a turn is active are queued and serialized with completed-worker notifications. Parent-turn cancellation cancels only the active turn; process shutdown cancels all active workers and their commands. Pi-style Agent Skills are input-context packages, not additional model tools: Lucy MAY discover their metadata at new-session boot and expand an explicit `/<name> [args]` user message into that skill's saved `SKILL.md` content, but it MUST NOT expose a skill tool or execute a skill itself. The LLM integration MUST target the OpenAI-compatible Chat Completions API, while keeping the base URL configurable.

Provider SSE and tool-call chunks MUST be converted into Lucy-owned normalized events. Provider-specific response chunks MUST NOT become the public JSONL protocol or TUI output. JSONL mode MUST emit only newline-delimited events on stdout and diagnostics to stderr. The TUI MUST render the same normalized event sequence, including streamed assistant deltas, tool calls/results, errors, normal turn completion, and user interruption. One process handles one active turn at a time.
Lucy MUST NOT impose a fixed count or provider-round limit on model tool calls within an active main-agent turn or spawned worker. Resource bounds remain in force for provider SSE bodies, tool-call fields and arguments, command execution time/output, concurrent spawned workers, cancellation, and process shutdown.

A spawned worker MUST be represented as a first-class child session linked to its parent session by `parent_session_id`. The child MUST persist its full secret-redacted transcript and terminal status in a separate append-only JSONL session file while retaining the one-process, same-cwd execution model. Child sessions MUST use only the boot snapshot plus their delegated task as initial model context; they MUST NOT inherit the parent transcript or tool output. The first implementation MUST NOT resume a running child after Lucy restarts, but MUST record process-shutdown interruption before graceful process exit when possible.

The harness MUST expose child lifecycle control for `wait_subagent`, `send_subagent`, and `cancel_subagent` in addition to `spawn_subagent` and `check_subagent`. `send_subagent` MUST queue input for the child's next safe provider boundary without interrupting an in-flight provider stream or command. `cancel_subagent` MUST cancel provider and command work at their nearest safe cancellation point. `wait_subagent` MUST wait for a terminal child state without canceling the child when its wait timeout expires.

A completed child notification MUST NOT interrupt an active main provider stream or command. After the current provider response's complete tool-call batch and results are persisted, the harness MUST drain ready child completions before the next provider request. Each completion MUST be appended as a separate parent user message in completion order; all ready completions MUST be included in one next provider request. Child completions take precedence over queued user messages at this safe boundary.

Automatic context compaction MUST run only at a safe boundary inside that active turn: after a provider response and all associated `cmd` results are complete and persisted, and before the next provider request. It MUST NOT interrupt an in-flight provider SSE stream or execute tools from the compaction-summary request. Compaction starts when the estimated context reaches at least 95% of the model window, uses the configured model with no tools, and continues the original turn only after a successful summary and context replacement. A failed or canceled compaction MUST NOT persist a summary or replacement boundary and MUST NOT emit normal turn completion.

ESC cancellation MUST stop a provider stream or running `cmd` at the nearest safe cancellation point. A canceled command's process group MUST be terminated when possible. Cancellation MUST emit a normalized interruption event and MUST NOT emit a normal turn completion event.

## Context and forces

The goal is a thin, embeddable harness rather than a full coding-agent product. A local trusted model needs command execution and conversation state, but callers should not depend on OpenAI/OpenRouter chunk shapes. A local process boundary keeps integration simple without adding authentication or multi-tenant concerns.

## Invariants

- Machine input messages and output events are LF-delimited JSON records.
- A successful turn exposes assistant deltas, normalized tool calls/results, and an explicit turn completion event.
- An interrupted turn exposes all safe events emitted before cancellation and one interruption event; it does not claim normal completion.
- A model tool call is executed by the harness before the next provider turn, except that `spawn_subagent` acknowledges queued work immediately.
- Active main-agent and spawned-worker tool loops have no fixed tool-call count or provider-round ceiling; they stop on a provider completion, cancellation, provider failure, or another existing resource boundary.
- `cmd`, `spawn_subagent`, `check_subagent`, `wait_subagent`, `send_subagent`, and `cancel_subagent` are the only model-facing tools. A spawned worker receives only `cmd`; it cannot recursively delegate.
- The harness executes no more than four spawned workers concurrently. Each worker inherits the session cwd and immutable boot context, but not parent transcript or tool output; its task is the complete delegated context.
- A spawned worker's selected model and optional effort override affect only that worker request; it uses the session provider endpoint and credential. Its completion is persisted in the child session and queued as a secret-redacted parent message; an active main turn consumes it before its next provider request, otherwise it triggers a serialized follow-up turn. `check_subagent`, `wait_subagent`, `send_subagent`, and `cancel_subagent` report or control the current in-process task state.
- While a provider turn is active, user messages and completed-worker notifications are queued. The harness never has more than one active provider turn, and drains queued work serially.
- A skill invocation is a user-message expansion, not a tool call or public protocol event. Lucy-owned slash commands are TUI controls and never enter that expansion path.
- A new session discovers skills from configured built-in locations using symlink-safe reads, catalogs only metadata for model selection, and persists the secret-redacted skill snapshot. A resumed session invokes only that snapshot and never rereads changed skill paths.
- Automatic compaction is an internal control phase, not a new public JSONL event or model-facing tool capability; TUI-only progress may use an internal frontend signal.
- TUI completion notifications are frontend-only OSC 777 signals, are emitted at most once when an active turn releases `busy`, cover completion/interruption/error outcomes, and use fixed Lucy-owned secret-safe text.
- Live background-worker streams, the between-input-and-status list, and the focused-worker stream overlay are TUI-only frontend surfaces. They are projected from harness-internal worker activity and MUST NOT become public JSONL protocol events.
- A successful `spawn_subagent` tool result settles its transcript card as completed when the worker is created; worker completion remains a separate queued main-agent notification path.
- `check_subagent` shares the compact one-line tool-card presentation used by `cmd` and `spawn_subagent`.
- Compaction never treats a partial assistant/tool-call stream as a complete context record, and its summary request has no `cmd` tool definition.
- The active provider key is not emitted in protocol events, TUI output, or diagnostics; key values that cannot be safely represented are rejected before output. TUI mode rejects keys containing fixed terminal input/border characters.
- No network listener, authentication layer, approval UI, or sandbox is required by this boundary.

Tool loops may continue for an arbitrary number of provider rounds until the model completes or an existing cancellation/resource boundary stops them. This removes an artificial count ceiling while retaining bounds on each provider payload, command, worker, and cancellation path.

## Alternatives and trade-offs

A library, HTTP server, or raw provider-stream pass-through would increase coupling or implementation surface. Additional file tools would make Lucy a larger coding agent and are intentionally left to future extensions or callers. A bounded non-recursive delegation tool permits parallel specialized work without turning the process into a network service or exposing the parent transcript to every worker. Treating skills as model tools would expand the protocol and execution surface; progressive-disclosure context plus explicit user invocation preserves the one-tool boundary while supporting reusable workflows.

## Consequences

Interactive users receive a terminal chat experience by default, while scripts retain an explicit and automatic JSONL path. Clients must implement a small event consumer, but they remain independent of provider wire formats. Skill command text works through the existing message input in both frontends and needs no protocol extension. The harness serializes one active provider turn at a time while a serialized queue admits user messages and completed worker notifications. Background delegation can incur automatic follow-up provider calls when a worker completes, but does not block the initiating turn. Compaction adds provider latency and a second model request at a safe turn boundary, but does not change the public protocol or expose another tool. A cancellation can leave a valid partial transcript and interruption record rather than a normal completed turn; cancellation during compaction follows the same rule without persisting a partial summary. Supported terminal emulators may surface a fixed-text desktop notification when the TUI turn ends, while unsupported terminals and JSONL clients remain unaffected.

## Enforcement

Integration tests MUST exercise TTY and non-TTY mode selection, JSONL input/output, normalized text streaming, the cmd and subagent lifecycle tool loops, explicit skill invocation and snapshot persistence, stdout purity, and interruption ordering. Tests MUST prove that workers receive only their delegated task plus boot context, inherit or override model/effort as requested, cannot recursively delegate, run no more than four concurrently, are acknowledged without blocking the parent, produce automatic queued completion follow-ups, support status inspection, and are canceled on process shutdown. Tests MUST verify that a provider-specific stream is not forwarded as a public event, that ESC cancels provider and command work within bounded behavior, and that the TUI reflects the normalized event sequence. Compaction tests MUST verify that summary requests expose no tools, occur only after complete provider/cmd boundaries, do not add public JSONL records, and leave the turn/session valid on failure or cancellation. TUI tests MUST verify slash-picker discovery and dispatch for Lucy-owned commands, settings argument handling, `/exit`, model-list viewport tracking, and advertised effort selection. They MUST verify the between-input-and-status worker list, Down/Up/Esc focus movement, focused-worker stream overlay placement, successful `spawn_subagent` card completion on create, and compact `check_subagent` card rendering. They MUST also verify that completion, interruption, and error paths each emit one fixed-text OSC 777 notification only when `busy` is released, and that notification write failures do not change the turn result.

## Revisit when

Reconsider this decision if callers require concurrent sessions in one process, a remote deployment, multiple providers with incompatible tool protocols, first-class file operations, recursively delegated or durable workers, a first-class skill execution protocol, or a different interactive frontend boundary.
