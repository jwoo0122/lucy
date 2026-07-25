---
id: harness.command-execution
status: accepted
scope: harness
decision_type: execution
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy executes trusted shell commands locally with bounded time and output, including process-scoped background execution.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
  - harness.session-and-context-lifecycle
supersedes: []
superseded_by: []
last_reviewed: "2026-07-26"
enforcement:
  - id: background-command-contract
    path: tests/cli.rs
    must_contain:
      - "fn background_cmd_completion_starts_an_automatic_turn_after_turn_end()"
      - "fn background_cmd_completion_is_delivered_before_the_active_turn_ends()"
    must_not_contain: []
  - id: bounded-command-execution
    path: src/command.rs
    must_contain:
      - "pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);"
      - "pub const COMMAND_OUTPUT_CAP: usize = 64 * 1024;"
    must_not_contain: []
  - id: background-completion-is-not-system-context
    path: src/app.rs
    must_contain:
      - "ChatMessage::observation(content)"
    must_not_contain:
      - "ChatMessage::system(content)"
  - id: observation-wire-mapping
    path: src/model.rs
    must_contain:
      - "fn observations_are_stored_apart_from_system_and_sent_as_user()"
      - "fn wire_role(&self) -> &str"
    must_not_contain: []
  - id: codex-observation-mapping
    path: src/codex_provider.rs
    must_contain:
      - "fn codex_request_keeps_observations_out_of_instructions()"
      - "\"role\": message.wire_role(),"
    must_not_contain: []
  - id: legacy-background-completion-demotion
    path: src/session.rs
    must_contain:
      - "fn demote_legacy_background_completion(message: &ChatMessage) -> ChatMessage"
      - "fn background_completions_reach_the_provider_as_observations_not_system_context()"
    must_not_contain: []
enforcement_exception: null
---

# Trusted local command execution

## Decision question

What execution semantics does the v1 `cmd` tool provide?

## Current decision

Lucy MUST target macOS/Linux in v1 and execute `cmd` arguments through the user environment's `$SHELL -lc`, falling back to `/bin/sh -lc` when `SHELL` is unset or empty. The command MUST run from the session's starting cwd and inherit the Lucy process environment, including the configured provider API-key environment variable. stdin MUST be disconnected. Lucy MUST support finite foreground commands and process-scoped background commands; interactive process management remains out of scope.

Each command MUST have a 10-minute timeout. A `cmd` call MAY set `background: true`; Lucy MUST then register the command internally, immediately return a stable background ID with running status, and execute the otherwise unchanged command concurrently. stdout and stderr MUST each be bounded to 64 KiB; truncation MUST be represented in the normalized tool result. A non-zero exit is a successful tool invocation with its exit code and captured output, not a harness-level protocol error.

Captured command output is untrusted data at every delivery position. Foreground output reaches the model as the originating call's tool result. Background completion has no valid tool position when it arrives, so Lucy MUST persist it under a distinct session-level observation role rather than the system role. Each provider adapter MUST map that role down to the lowest-authority role the provider accepts, and MUST NOT send it as system or developer authority. Sessions written before the observation role existed MUST be reinterpreted when the provider context is reconstructed, without rewriting the session file.

## Context and forces

The model is explicitly trusted and the harness is local-only, so v1 does not add approval, sandboxing, or an allowlist. Lucy treats the command environment as a trusted terminal environment and redacts the provider credential from captured/persisted tool output. The credential is therefore available to commands and may be observed by them; this does not provide OS-level isolation from parent-process inspection or transformed side channels. Bounds are required to prevent a hung or unbounded command from blocking the protocol or consuming the model context without limit.

Trusting the model to issue commands does not extend to trusting whatever those commands print. The Codex adapter joins every system message into the top-level `instructions`, so persisting a background completion as system context gave arbitrary program stdout the authority of the boot system prompt and erased its position in the conversation; OpenAI-compatible Chat Completions carries the same system role with the same effect. A distinct observation role removes that escalation for both adapters at once and keeps the completion in conversation order.

Role demotion bounds authority, not interpretation. Untrusted output delivered as a low-authority message can still be read as an instruction, so the completion is additionally wrapped in an explicit untrusted-output envelope. The envelope is a mitigation, not a boundary; the actual boundary would be an execution-approval layer, which this decision deliberately leaves out of scope.

## Invariants

- The shell command string is passed without Lucy-side rewriting.
- The shell executable is inherited from `SHELL` when set, with `/bin/sh` as the empty/unset fallback.
- The configured provider API-key variable remains available in the child environment.
- Captured command output is redacted before protocol or session serialization.
- The command cwd is stable across invocations; shell-local `cd` does not mutate the session cwd.
- Timeout and output truncation produce a tool result that the model can handle.
- After timeout or cancellation, Lucy terminates the shell's process group and stops waiting for capture after a bounded grace period.
- Descendants that deliberately escape the process group/session are outside the v1 containment boundary and may continue; any incomplete capture is marked truncated.
- Foreground command output and exit status are persisted as part of the originating conversation turn.
- Background completion is persisted as a Lucy-owned untrusted observation containing the background ID, the normalized command result, and an explicit untrusted-output envelope.
- No provider adapter promotes an observation to system or developer authority; every adapter maps it to the lowest-authority role the provider accepts, and it keeps its position in the conversation.
- Background completions persisted as system context by earlier Lucy versions are demoted to observations when the provider context is reconstructed; the session file is not rewritten.
- A completed background command is delivered at the earliest provider-response boundary. If the originating user turn already ended, Lucy starts an automatic turn without waiting for another user message.
- Active background commands are process-scoped: they continue across user-turn cancellation, are canceled when Lucy exits, and are not reconstructed when a session is resumed.
- JSONL one-shot mode remains alive after input EOF while registered background commands are running so their completion can be delivered.

## Alternatives and trade-offs

A dedicated argv API would reduce shell interpretation but would not satisfy the intended command-line agent experience. A fixed `/bin/sh` would be more predictable, but would ignore the user's configured shell and startup behavior. Interactive process support would enable REPLs but requires stdin multiplexing and a broader lifecycle API. Poll, list, stop, and restart recovery for background commands remain deferred.

Background completion could instead reuse the originating `cmd` tool result, but that call was already answered with the background ID, so a second result would duplicate a `call_id` that strict providers reject. Modeling messages as a closed role enum would make a missing adapter mapping a compile error, at the cost of breaking the persisted JSONL shape and every match site; per-adapter mapping tests were accepted as the equivalent guarantee. Migrating existing session files was rejected as an irreversible rewrite of append-only history when read-time demotion is sufficient.

## Consequences

Commands that exceed the timeout or output cap require the model to rerun them with narrower output or a shorter operation. Background completion can create an automatic provider request after control has returned to the user, but provider requests remain serialized. A trusted command can still inspect Lucy or exfiltrate transformed data outside the direct-output guarantee. The shell behavior is Unix-specific until a future platform decision is made.

Every future provider adapter must decide how to map the observation role; the session role set is no longer identical to any provider's wire role set. Command output can still attempt prompt injection at ordinary conversation authority, which this decision bounds but does not eliminate.

## Enforcement

Integration tests MUST cover successful commands, non-zero exit codes, inherited cwd, timeout termination, stdout/stderr capture, output truncation, immediate background registration, and automatic completion delivery before and after an originating turn ends. Tests MUST verify that a timed-out shell/process group is terminated and that escaped-descendant capture returns within the grace bound.

## Revisit when

Reconsider this decision if callers need Windows, interactive stdin, durable background-job recovery, explicit job polling/stopping, persistent shell state, or stronger isolation than a trusted local shell.
