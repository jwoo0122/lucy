---
id: harness.agent-boundary-and-protocol
status: accepted
scope: harness
decision_type: boundary
applies_to:
- Cargo.toml
- src/**
- tests/**
- README.md
- skills/**
summary: Lucy is a local harness with TUI, raw JSONL, and finite one-shot exec frontends over one turn engine while cmd remains its only model-facing tool.
constrains: []
depends_on: []
supersedes: []
superseded_by: []
last_reviewed: '2026-08-15'
enforcement:
- invariant: jsonl-record-framing
  kind: executable
  check: rust-tests
- invariant: successful-turn-events
  kind: executable
  check: rust-tests
- invariant: finite-one-shot-exec
  kind: executable
  check: rust-tests
- invariant: truthful-exec-status
  kind: executable
  check: rust-tests
- invariant: interruption-ordering
  kind: executable
  check: rust-tests
- invariant: command-before-provider-continuation
  kind: executable
  check: rust-tests
- invariant: unbounded-tool-round-count
  kind: executable
  check: rust-tests
- invariant: sole-model-tool
  kind: executable
  check: rust-tests
- invariant: caller-owned-session-orchestration
  kind: executable
  check: rust-tests
- invariant: observable-session-cwd
  kind: executable
  check: rust-tests
- invariant: stable-resume-context
  kind: executable
  check: rust-tests
- invariant: skills-are-message-expansions
  kind: executable
  check: rust-tests
- invariant: provider-key-confidentiality
  kind: executable
  check: rust-tests
- invariant: external-skill-distribution
  kind: executable
  check: rust-tests
- invariant: excluded-product-surfaces
  kind: manual
  reason: Absence of broad product surfaces cannot be established completely by portable behavioral tests.
  evidence:
  - Cargo.toml
  - src/app.rs
  - src/provider.rs
  - README.md#project-purpose
  revisit_when:
  - A repository architecture check can enumerate network listeners and privileged product surfaces.
invariants:
- id: jsonl-record-framing
  statement: Machine input messages and output events are LF-delimited JSON records.
- id: successful-turn-events
  statement: A successful raw JSONL turn exposes assistant deltas, normalized cmd calls and results, and an explicit turn completion event.
- id: finite-one-shot-exec
  statement: lucy exec accepts exactly one task, runs it through the existing turn engine, and emits either final assistant text or one aggregate JSON value before exiting.
- id: truthful-exec-status
  statement: lucy exec exits successfully only when its submitted turn reaches successful completion; interrupted or otherwise unsuccessful turns exit nonzero.
- id: interruption-ordering
  statement: An interrupted turn exposes all safe events emitted before cancellation and one interruption event without claiming normal completion.
- id: command-before-provider-continuation
  statement: A model cmd call is executed by the harness before the next provider turn.
- id: unbounded-tool-round-count
  statement: Model tool loops have no fixed provider-round limit and stop only on completion or an existing cancellation or resource boundary.
- id: sole-model-tool
  statement: cmd is the only model-facing tool.
- id: caller-owned-session-orchestration
  statement: Lucy never creates, links, resumes, or delivers internal subagent sessions; callers own named session IDs and process orchestration.
- id: observable-session-cwd
  statement: The session event exposes saved and effective working directories and whether cwd fallback occurred.
- id: stable-resume-context
  statement: A resumed session reconstructs the same immutable boot context and append-only conversation state as the original process.
- id: skills-are-message-expansions
  statement: A skill invocation is a user-message expansion, not a tool call or public protocol event.
- id: provider-key-confidentiality
  statement: The active provider key is absent from protocol events, TUI output, diagnostics, and persisted records, and unsafe key values are rejected before output.
- id: external-skill-distribution
  statement: Lucy distributes its compile-time embedded caller-facing skill byte-for-byte without bootstrap, filesystem mutation, or runtime delegation authority.
- id: excluded-product-surfaces
  statement: Lucy does not add a network listener, authentication layer, approval UI, sandbox, internal delegation scheduler, or cross-session relationship metadata.
---

# Local interactive, raw JSONL, and one-shot exec harness boundary

## Decision question

What public boundary and capability surface does the Lucy harness expose to interactive users and machine clients?

## Current decision

Lucy MUST run as a local macOS/Linux process and MUST retain its newline-delimited JSON machine protocol. When both standard input and standard output are terminals, an invocation without a mode flag MUST start the TUI. When either stream is not a terminal, the invocation MUST use JSONL automatically. `--jsonl` MUST force JSONL and `--tui` MUST force the interactive frontend; the latter requires a usable terminal. The TUI is a frontend over the same normalized event and turn engine, not a new provider or tool boundary. Its slash picker MUST combine discovered skill names with Lucy-owned `/settings` and `/exit` commands without persisting or expanding those commands as skills. `/settings` MUST ignore trailing arguments and open the idle-only settings menu; `/exit` MUST terminate an idle TUI session.

Lucy MUST expose only `cmd` as a model-facing tool and MUST NOT provide built-in `read`, `write`, `edit`, delegation, lifecycle, or other file-operation tools. Lucy MUST NOT be a daemon or network service in v1. The LLM integration MUST support the configurable OpenAI-compatible Chat Completions API and MAY use the explicit authenticated Codex subscription adapter. Provider-specific authentication MUST remain outside the model-facing protocol.

The raw JSONL interface MUST accept newline-delimited `{"type":"message","text":"..."}` records and MUST emit only newline-delimited normalized events on stdout, with diagnostics on stderr. A normal client interaction MUST expose a `session` event, streamed assistant deltas, normalized `cmd` calls/results, and one `turn_end` event. The `session` event MUST report the effective cwd, saved cwd, and whether cwd fallback occurred so a machine caller can detect execution outside the session's original workspace. A client MAY close stdin after one message; Lucy MUST finish that turn and exit after EOF. A client MAY resume a named session with `--session <id>` and send another message.

`lucy exec` MUST be a finite one-shot frontend over that same turn engine, not a second execution engine. It MUST accept exactly one task, wait for that turn to reach a terminal outcome, and then exit. Its selected output MUST be either the final assistant text or exactly one aggregate JSON value. The aggregate JSON value MUST be one complete result object reporting final status, session ID, effective cwd, saved cwd, cwd-fallback status, and assistant text. Failed exec turns MUST keep stdout empty and report diagnostics on stderr. Text mode MUST keep stdout to final assistant text and send diagnostics to stderr. An exec process MUST exit zero only for a successfully completed turn and MUST exit nonzero for interruption, provider failure, persistence failure, invalid invocation, or any other unsuccessful submitted turn.

Session identity and process lifetime remain caller-managed for every frontend. Lucy MUST NOT infer parent/child relationships between sessions or gain a daemon, network-facing orchestration API, internal delegation scheduler, child-agent lifecycle, or cross-session result delivery. External subagent discovery and invocation guidance MAY be distributed as a caller-facing skill, but that skill does not add runtime delegation authority to Lucy. `lucy skill` MUST emit the exact caller-facing skill embedded at compile time, without bootstrap or filesystem mutation, and MUST reject additional arguments.

Provider SSE and tool-call chunks MUST be converted into Lucy-owned normalized events. Provider-specific response chunks MUST NOT become the public raw JSONL protocol, exec output, or TUI output. One process handles one active turn at a time.
Lucy MUST NOT impose a fixed count or provider-round limit on model tool calls within an active turn. Resource bounds remain in force for provider SSE bodies, tool-call fields and arguments, command execution time/output, cancellation, and process shutdown.

Pi-style Agent Skills are input-context packages, not additional model tools: Lucy MAY discover their metadata at new-session boot and expand an explicit `/<name> [args]` user message into that skill's saved `SKILL.md` content, but it MUST NOT expose a skill tool or execute a skill itself.

## Context and forces

The goal is a thin, embeddable harness rather than a full coding-agent product. A local trusted model needs command execution and conversation state, but callers should not depend on OpenAI/OpenRouter chunk shapes. A local process boundary keeps integration simple. Independent agents can communicate by invoking Lucy as a finite subprocess, choosing raw JSONL or one-shot exec as appropriate, and explicitly managing returned session IDs; Lucy does not need an internal worker or process relationship model. A separately distributed discovery skill can teach that caller-side composition without changing the runtime boundary.

## Invariants

- Machine input messages and output events are LF-delimited JSON records.
- A successful raw JSONL turn exposes assistant deltas, normalized cmd calls and results, and an explicit turn completion event.
- `lucy exec` accepts exactly one task, uses the existing turn engine, and emits either final assistant text or one aggregate JSON value before exiting.
- `lucy exec` exits successfully only for a successfully completed submitted turn; interruption and other turn failures exit nonzero.
- An interrupted turn exposes all safe events emitted before cancellation and one interruption event without claiming normal completion.
- A model cmd call is executed by the harness before the next provider turn.
- Model tool loops have no fixed provider-round limit and stop only on completion or an existing cancellation or resource boundary.
- cmd is the only model-facing tool.
- Lucy never creates, links, resumes, or delivers internal subagent sessions; callers own named session IDs and process orchestration.
- The session event exposes saved and effective working directories and whether cwd fallback occurred.
- A resumed session reconstructs the same immutable boot context and append-only conversation state as the original process.
- A skill invocation is a user-message expansion, not a tool call or public protocol event.
- The active provider key is absent from protocol events, TUI output, diagnostics, and persisted records, and unsafe key values are rejected before output.
- `lucy skill` distributes the compile-time embedded external caller-facing skill byte-for-byte without mutation or runtime delegation authority.
- Lucy does not add a daemon, network listener, network-facing orchestration API, authentication layer, approval UI, sandbox, internal delegation scheduler, or cross-session relationship metadata.

## Alternatives and trade-offs

A library, daemon, HTTP server, or raw provider-stream pass-through would increase coupling or implementation surface. Additional file tools would make Lucy a larger coding agent and are intentionally left to callers. Keeping delegation inside Lucy would require worker lifecycle, result delivery, and relationship persistence. Lucy instead exposes stable finite-process and raw-JSONL/session boundaries so callers can launch multiple independent instances. Keeping raw JSONL as the only automation frontend would force simple callers to consume a stream and infer turn success. One-shot exec adds a small aggregate adapter plus process startup and session-file I/O, but preserves the finite process contract, reports turn failure truthfully, and isolates failures between agents.

## Consequences

Interactive users receive a terminal chat experience, streaming clients retain the raw JSONL path, and finite callers may use text or single-value aggregate exec output. JSONL clients must implement a small event consumer; exec clients must inspect process status and retain session IDs when they want continuity. Multiple Lucy processes remain caller-orchestrated, and same-session mutable access follows the writer-lease guarantee in the session lifecycle record. OpenRouter credentials remain environment-based, while Codex subscription credentials remain in Lucy’s private credential store and are not exposed to model commands.

## Enforcement

Integration tests MUST exercise TTY and non-TTY mode selection, raw JSONL input/output, exec final-text and single-value JSON output, successful and unsuccessful exec process status, normalized text streaming, the `cmd` tool loop, explicit skill invocation and snapshot persistence, stdout purity, session creation/resume across separate processes, observable cwd fallback, and interruption ordering. Provider tests MUST verify that `cmd` remains the only model-facing tool. Tests MUST also verify that provider-specific streams are not forwarded as public events, no subagent/background-result protocol or session records are emitted, and a `cmd` child inherits ordinary environment variables while the configured provider key is removed and credentials remain absent from protocol, diagnostics, and persisted records. Compaction tests MUST verify that summary requests expose no tools and occur only at complete provider/cmd boundaries.

## Revisit when

Reconsider this decision if callers require multiple submitted tasks in one exec process, a nonterminal exec lifecycle, output beyond final text or one aggregate JSON value, concurrent sessions in one process, a daemon or remote orchestration deployment, additional providers with incompatible tool protocols, first-class file operations, a durable cross-session relationship protocol, or a different interactive frontend boundary.
