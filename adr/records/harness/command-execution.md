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
last_reviewed: "2026-07-22"
enforcement:
  - id: background-command-contract
    path: tests/cli.rs
    must_contain:
      - "fn background_cmd_completion_starts_an_automatic_turn_after_turn_end()"
      - "fn background_cmd_completion_is_delivered_before_the_active_turn_ends()"
    must_not_contain: []
  - id: background-completion-privilege
    path: src/app.rs
    must_contain:
      - "ChatMessage::observation(content)"
      - "fn background_completion_delimiter_cannot_be_forged_by_command_output()"
    must_not_contain:
      - "ChatMessage::system(content)"
  - id: observation-message-privilege
    path: src/model.rs
    must_contain:
      - 'pub const OBSERVATION_ROLE: &str = "observation";'
      - "fn observation_keeps_its_session_role_but_uses_the_openai_user_role()"
    must_not_contain: []
  - id: codex-observation-privilege
    path: src/codex_provider.rs
    must_contain:
      - "fn codex_request_maps_observations_to_unprivileged_user_input()"
    must_not_contain: []
  - id: bounded-command-execution
    path: src/command.rs
    must_contain:
      - "pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);"
      - "pub const COMMAND_OUTPUT_CAP: usize = 64 * 1024;"
    must_not_contain: []
enforcement_exception: null
---

# Trusted local command execution

## Decision question

What execution semantics does the v1 `cmd` tool provide?

## Current decision

Lucy MUST target macOS/Linux in v1 and execute `cmd` arguments through the user environment's `$SHELL -lc`, falling back to `/bin/sh -lc` when `SHELL` is unset or empty. The command MUST run from the session's starting cwd and inherit the Lucy process environment, including the configured provider API-key environment variable. stdin MUST be disconnected. Lucy MUST support finite foreground commands and process-scoped background commands; interactive process management remains out of scope.

Each command MUST have a 10-minute timeout. A `cmd` call MAY set `background: true`; Lucy MUST then register the command internally, immediately return a stable background ID with running status, and execute the otherwise unchanged command concurrently. stdout and stderr MUST each be bounded to 64 KiB; truncation MUST be represented in the normalized tool result. A non-zero exit is a successful tool invocation with its exit code and captured output, not a harness-level protocol error. A completed background command MUST re-enter the conversation as a low-privilege observation message rather than as system context, because command output is untrusted data that MUST NOT gain system-instruction authority in any provider request.

## Context and forces

The model is explicitly trusted and the harness is local-only, so v1 does not add approval, sandboxing, or an allowlist. Lucy treats the command environment as a trusted terminal environment and redacts the provider credential from captured/persisted tool output. The credential is therefore available to commands and may be observed by them; this does not provide OS-level isolation from parent-process inspection or transformed side channels. Bounds are required to prevent a hung or unbounded command from blocking the protocol or consuming the model context without limit.

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
- Background completion is persisted as a Lucy-owned low-privilege observation message containing the background ID and normalized command result; it is never persisted or sent as system/instruction context, and its payload is framed as untrusted data inside a per-message randomly nonced delimiter that captured output cannot forge.
- Each provider adapter maps an observation to the lowest available input privilege: the OpenAI-compatible adapter sends it as a `user` message and the Codex adapter sends it as a `user` input item, never as part of `instructions`.
- Legacy sessions that recorded a background completion as a system message are downgraded to an observation when provider messages are reconstructed, without rewriting the session file.
- A completed background command is delivered at the earliest provider-response boundary. If the originating user turn already ended, Lucy starts an automatic turn without waiting for another user message.
- Active background commands are process-scoped: they continue across user-turn cancellation, are canceled when Lucy exits, and are not reconstructed when a session is resumed.
- JSONL one-shot mode remains alive after input EOF while registered background commands are running so their completion can be delivered.

## Alternatives and trade-offs

A dedicated argv API would reduce shell interpretation but would not satisfy the intended command-line agent experience. A fixed `/bin/sh` would be more predictable, but would ignore the user's configured shell and startup behavior. Interactive process support would enable REPLs but requires stdin multiplexing and a broader lifecycle API. Poll, list, stop, and restart recovery for background commands remain deferred.

## Consequences

A background completion can no longer carry harness-level authority, so a model that treats delimited command output as an instruction is a model-side failure rather than a harness-granted privilege. Commands that exceed the timeout or output cap require the model to rerun them with narrower output or a shorter operation. Background completion can create an automatic provider request after control has returned to the user, but provider requests remain serialized. A trusted command can still inspect Lucy or exfiltrate transformed data outside the direct-output guarantee. The shell behavior is Unix-specific until a future platform decision is made.

## Enforcement

Integration tests MUST cover successful commands, non-zero exit codes, inherited cwd, timeout termination, stdout/stderr capture, output truncation, immediate background registration, and automatic completion delivery before and after an originating turn ends. Tests MUST verify that a timed-out shell/process group is terminated and that escaped-descendant capture returns within the grace bound.

## Revisit when

Reconsider this decision if callers need Windows, interactive stdin, durable background-job recovery, explicit job polling/stopping, persistent shell state, or stronger isolation than a trusted local shell.
