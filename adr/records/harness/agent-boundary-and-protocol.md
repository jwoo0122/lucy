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
summary: Lucy is a local macOS/Linux harness with a default TUI, an automatic/explicit JSONL mode, one OpenAI-compatible provider, and one model-facing cmd tool.
constrains: []
depends_on: []
supersedes: []
superseded_by: []
last_reviewed: "2026-07-17"
---

# Local interactive and JSONL harness boundary

## Decision question

What public boundary and capability surface does the Lucy harness expose to interactive users and machine clients?

## Current decision

Lucy MUST run as a local macOS/Linux process and MUST retain its newline-delimited JSON machine protocol. When both standard input and standard output are terminals, an invocation without a mode flag MUST start the TUI. When either stream is not a terminal, the invocation MUST use JSONL automatically. `--jsonl` MUST force JSONL and `--tui` MUST force the interactive frontend; the latter requires a usable terminal. The TUI is a frontend over the same normalized event and turn engine, not a new provider or tool boundary.

Lucy MUST NOT be a network service in v1. The harness MUST expose only the `cmd` model tool; it MUST NOT provide built-in `read`, `write`, `edit`, or other file-operation tools. The LLM integration MUST target the OpenAI-compatible Chat Completions API, while keeping the base URL configurable.

Provider SSE and tool-call chunks MUST be converted into Lucy-owned normalized events. Provider-specific response chunks MUST NOT become the public JSONL protocol or TUI output. JSONL mode MUST emit only newline-delimited events on stdout and diagnostics to stderr. The TUI MUST render the same normalized event sequence, including streamed assistant deltas, tool calls/results, errors, normal turn completion, and user interruption. One process handles one active turn at a time.

ESC cancellation MUST stop a provider stream or running `cmd` at the nearest safe cancellation point. A canceled command's process group MUST be terminated when possible. Cancellation MUST emit a normalized interruption event and MUST NOT emit a normal turn completion event.

## Context and forces

The goal is a thin, embeddable harness rather than a full coding-agent product. A local trusted model needs command execution and conversation state, but callers should not depend on OpenAI/OpenRouter chunk shapes. A local process boundary keeps integration simple without adding authentication or multi-tenant concerns.

## Invariants

- Machine input messages and output events are LF-delimited JSON records.
- A successful turn exposes assistant deltas, normalized tool calls/results, and an explicit turn completion event.
- An interrupted turn exposes all safe events emitted before cancellation and one interruption event; it does not claim normal completion.
- A model tool call is executed by the harness before the next provider turn.
- The active provider key is not emitted in protocol events, TUI output, or diagnostics; key values that cannot be safely represented are rejected before output. TUI mode rejects keys containing fixed terminal input/border characters.
- No network listener, authentication layer, approval UI, or sandbox is required by this boundary.

## Alternatives and trade-offs

A library, HTTP server, or raw provider-stream pass-through would increase coupling or implementation surface. Additional file tools would make Lucy a larger coding agent and are intentionally left to future extensions or callers.

## Consequences

Interactive users receive a terminal chat experience by default, while scripts retain an explicit and automatic JSONL path. Clients must implement a small event consumer, but they remain independent of provider wire formats. The single active turn rule avoids multiplexing and ordering semantics in the first protocol. A cancellation can leave a valid partial transcript and interruption record rather than a normal completed turn.

## Enforcement

Integration tests MUST exercise TTY and non-TTY mode selection, JSONL input/output, normalized text streaming, the cmd tool loop, stdout purity, and interruption ordering. Tests MUST verify that a provider-specific stream is not forwarded as a public event, that ESC cancels provider and command work within bounded behavior, and that the TUI reflects the normalized event sequence.

## Revisit when

Reconsider this decision if callers require concurrent sessions in one process, a remote deployment, multiple providers with incompatible tool protocols, first-class file operations, or a different interactive frontend boundary.
