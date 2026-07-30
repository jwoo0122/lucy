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
summary: Lucy is a local macOS/Linux harness with TUI and JSONL interfaces while cmd remains its only model-facing tool.
constrains: []
depends_on: []
supersedes: []
superseded_by: []
last_reviewed: "2026-07-29"
enforcement:
  - id: cmd-only-tool-schema
    path: src/provider.rs
    must_contain:
      - "fn normal_requests_expose_only_cmd()"
      - '"name": "cmd"'
    must_not_contain: []
  - id: normalized-jsonl-tool-loop
    path: tests/cli.rs
    must_contain:
      - "fn streams_normalized_events_runs_cmd_loop_and_keeps_stdout_pure()"
    must_not_contain: []
  - id: codex-cmd-tool-schema
    path: src/codex_provider.rs
    must_contain:
      - "fn codex_request_uses_responses_shape()"
      - '"background":{"type":"boolean","default":false}'
    must_not_contain: []
enforcement_exception: null
---

# Local interactive and JSONL harness boundary

## Decision question

What public boundary and capability surface does the Lucy harness expose to interactive users and machine clients?

## Current decision

Lucy MUST run as a local macOS/Linux process and MUST retain its newline-delimited JSON machine protocol. When both standard input and standard output are terminals, an invocation without a mode flag MUST start the TUI. When either stream is not a terminal, the invocation MUST use JSONL automatically. `--jsonl` MUST force JSONL and `--tui` MUST force the interactive frontend; the latter requires a usable terminal. The TUI is a frontend over the same normalized event and turn engine, not a new provider or tool boundary. Its slash picker MUST combine discovered skill names with Lucy-owned `/settings` and `/exit` commands without persisting or expanding those commands as skills. `/settings` MUST ignore trailing arguments and open the idle-only settings menu; `/exit` MUST terminate an idle TUI session.

Lucy MUST expose only `cmd` as a model-facing tool and MUST NOT provide built-in `read`, `write`, `edit`, delegation, lifecycle, or other file-operation tools. Lucy MUST NOT be a network service in v1. The LLM integration MUST support the configurable OpenAI-compatible Chat Completions API and MAY use the explicit authenticated Codex subscription adapter. Provider-specific authentication MUST remain outside the model-facing protocol.

The JSONL interface MUST accept newline-delimited `{"type":"message","text":"..."}` records and MUST emit only newline-delimited normalized events on stdout, with diagnostics on stderr. A normal client interaction MUST expose a `session` event, streamed assistant deltas, normalized `cmd` calls/results, and one `turn_end` event. A client MAY close stdin after one message; Lucy MUST finish that turn and exit after EOF. A client MAY resume a named session with `--session <id>` and send another message. Session identity and process lifetime are caller-managed; Lucy MUST NOT infer parent/child relationships between sessions.

Provider SSE and tool-call chunks MUST be converted into Lucy-owned normalized events. Provider-specific response chunks MUST NOT become the public JSONL protocol or TUI output. One process handles one active turn at a time.
Lucy MUST NOT impose a fixed count or provider-round limit on model tool calls within an active turn. Resource bounds remain in force for provider SSE bodies, tool-call fields and arguments, command execution time/output, cancellation, and process shutdown.

Pi-style Agent Skills are input-context packages, not additional model tools: Lucy MAY discover their metadata at new-session boot and expand an explicit `/<name> [args]` user message into that skill's saved `SKILL.md` content, but it MUST NOT expose a skill tool or execute a skill itself.

## Context and forces

The goal is a thin, embeddable harness rather than a full coding-agent product. A local trusted model needs command execution and conversation state, but callers should not depend on OpenAI/OpenRouter chunk shapes. A local process boundary keeps integration simple. Independent agents can communicate by invoking Lucy as a finite JSONL subprocess and explicitly managing the returned session ID; Lucy does not need an internal worker or process relationship model.

## Invariants

- Machine input messages and output events are LF-delimited JSON records.
- A successful turn exposes assistant deltas, normalized `cmd` calls/results, and an explicit turn completion event.
- An interrupted turn exposes all safe events emitted before cancellation and one interruption event; it does not claim normal completion.
- A model `cmd` call is executed by the harness before the next provider turn.
- Model tool loops may continue for an arbitrary number of provider rounds until the model completes or an existing cancellation/resource boundary stops them.
- `cmd` is the only model-facing tool; Lucy never creates, links, resumes, or delivers internal subagent sessions.
- Named persistent sessions remain independently addressable through `--session <id>`; the caller owns session IDs and process orchestration.
- A new process resuming a session reconstructs the same immutable boot context and append-only conversation state as the original process.
- A skill invocation is a user-message expansion, not a tool call or public protocol event.
- The active provider key is not emitted in protocol events, TUI output, or diagnostics; key values that cannot be safely represented are rejected before output.
- Lucy does not add a network listener, authentication layer, approval UI, sandbox, internal delegation scheduler, or cross-session relationship metadata.

## Alternatives and trade-offs

A library, HTTP server, or raw provider-stream pass-through would increase coupling or implementation surface. Additional file tools would make Lucy a larger coding agent and are intentionally left to callers. Keeping delegation inside Lucy would require worker lifecycle, result delivery, and relationship persistence. Lucy instead exposes a stable process and JSONL/session boundary so callers can launch multiple independent instances. One-shot subprocess invocation adds process startup and session-file I/O, but preserves the finite `cmd` contract and isolates failures between agents.

## Consequences

Interactive users receive a terminal chat experience, while scripts and other agents retain an explicit and automatic JSONL path. Clients must implement a small event consumer and retain session IDs when they want continuity. Multiple Lucy processes may operate on different sessions without Lucy coordinating them. Concurrent writes to one session remain outside the lifecycle guarantee. OpenRouter credentials remain environment-based, while Codex subscription credentials remain in Lucy’s private credential store and are not exposed to model commands.

## Enforcement

Integration tests MUST exercise TTY and non-TTY mode selection, JSONL input/output, normalized text streaming, the `cmd` tool loop, explicit skill invocation and snapshot persistence, stdout purity, session creation/resume across separate processes, and interruption ordering. Provider tests MUST verify that `cmd` remains the only model-facing tool. Tests MUST also verify that provider-specific streams are not forwarded as public events, no subagent/background-result protocol or session records are emitted, and a `cmd` child inherits ordinary environment variables while the configured provider key is removed and credentials remain absent from protocol, diagnostics, and persisted records. Compaction tests MUST verify that summary requests expose no tools and occur only at complete provider/cmd boundaries.

## Revisit when

Reconsider this decision if callers require concurrent sessions in one process, a remote deployment, additional providers with incompatible tool protocols, first-class file operations, a durable cross-session relationship protocol, or a different interactive frontend boundary.
