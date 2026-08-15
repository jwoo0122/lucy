---
id: harness.command-execution
status: accepted
scope: harness
decision_type: execution
applies_to:
- src/**
- tests/**
- README.md
summary: Lucy executes trusted shell commands locally with bounded time and output, including process-scoped background execution.
constrains: []
depends_on:
- harness.agent-boundary-and-protocol
- harness.session-and-context-lifecycle
supersedes: []
superseded_by: []
last_reviewed: '2026-08-04'
enforcement:
- invariant: verbatim-shell-command
  kind: executable
  check: rust-tests
- invariant: configured-shell-with-fallback
  kind: executable
  check: rust-tests
- invariant: credential-stripped-environment
  kind: executable
  check: rust-tests
- invariant: shell-startup-outside-credential-boundary
  kind: manual
  reason: User shell startup behavior is external to the repository and cannot be deterministically verified here.
  evidence:
  - src/command.rs
  - adr/records/harness/command-execution.md#context-and-forces
  revisit_when:
  - Lucy stops invoking a user-configured login shell or adds an isolated execution boundary.
- invariant: redacted-captured-output
  kind: executable
  check: rust-tests
- invariant: stable-command-cwd
  kind: executable
  check: rust-tests
- invariant: bounded-result-semantics
  kind: executable
  check: rust-tests
- invariant: bounded-process-group-shutdown
  kind: executable
  check: rust-tests
- invariant: escaped-descendants-outside-containment
  kind: executable
  check: rust-tests
- invariant: foreground-result-persistence
  kind: executable
  check: rust-tests
- invariant: unprivileged-background-observation
  kind: executable
  check: rust-tests
- invariant: provider-observation-downgrade
  kind: executable
  check: rust-tests
- invariant: legacy-background-downgrade
  kind: executable
  check: rust-tests
- invariant: earliest-background-delivery
  kind: executable
  check: rust-tests
- invariant: process-scoped-background-lifetime
  kind: executable
  check: rust-tests
- invariant: jsonl-eof-waits-for-background
  kind: executable
  check: rust-tests
invariants:
- id: verbatim-shell-command
  statement: Lucy passes the shell command string without Lucy-side rewriting.
- id: configured-shell-with-fallback
  statement: Lucy uses SHELL when set and falls back to /bin/sh when SHELL is empty or unset.
- id: credential-stripped-environment
  statement: The configured provider API-key variable is removed from command children while the rest of the inherited environment is preserved.
- id: shell-startup-outside-credential-boundary
  statement: Lucy does not parse or sanitize shell startup behavior, which may independently reintroduce or retrieve credentials.
- id: redacted-captured-output
  statement: Captured command output is redacted before protocol or session serialization.
- id: stable-command-cwd
  statement: Command invocations retain the session cwd and shell-local cd does not mutate it.
- id: bounded-result-semantics
  statement: Timeout and output truncation produce a normalized tool result that the model can handle.
- id: bounded-process-group-shutdown
  statement: After timeout or cancellation, Lucy terminates the shell process group and stops waiting for capture after a bounded grace period.
- id: escaped-descendants-outside-containment
  statement: Descendants that escape the process group or session are outside the v1 containment boundary, and incomplete capture is marked truncated.
- id: foreground-result-persistence
  statement: Foreground command output and exit status are persisted as part of the originating conversation turn.
- id: unprivileged-background-observation
  statement: Background completion is persisted as a non-forgeable low-privilege observation and never as system or instruction context.
- id: provider-observation-downgrade
  statement: Each provider adapter maps observations to its lowest available user-input privilege.
- id: legacy-background-downgrade
  statement: Legacy background-completion system messages are downgraded during provider-message reconstruction without rewriting session files.
- id: earliest-background-delivery
  statement: A completed background command is delivered at the earliest provider-response boundary and starts an automatic turn if its originating turn already ended.
- id: process-scoped-background-lifetime
  statement: Background commands survive user-turn cancellation, stop when Lucy exits, and are not reconstructed on session resume.
- id: jsonl-eof-waits-for-background
  statement: JSONL one-shot mode remains alive after input EOF while registered background commands are running.
---

# Trusted local command execution

## Decision question

What execution semantics does the v1 `cmd` tool provide?

## Current decision

Lucy MUST target macOS/Linux in v1 and execute `cmd` arguments through the user environment's `$SHELL -lc`, falling back to `/bin/sh -lc` when `SHELL` is unset or empty. The command MUST run from the session's starting cwd and inherit the Lucy process environment except for the configured provider API-key environment variable, which MUST be removed before spawn. stdin MUST be disconnected. Lucy MUST support finite foreground commands and process-scoped background commands; interactive process management remains out of scope.

Each command MUST have a 10-minute timeout. A `cmd` call MAY set `background: true`; Lucy MUST then register the command internally, immediately return a stable background ID with running status, and execute the otherwise unchanged command concurrently. stdout and stderr MUST each be bounded to 64 KiB; truncation MUST be represented in the normalized tool result. A non-zero exit is a successful tool invocation with its exit code and captured output, not a harness-level protocol error. A completed background command MUST re-enter the conversation as a low-privilege observation message rather than as system context, because command output is untrusted data that MUST NOT gain system-instruction authority in any provider request.

## Context and forces

The model is explicitly trusted and the harness is local-only, so v1 does not add approval, sandboxing, or an allowlist. Lucy treats the command environment as a trusted terminal environment and redacts the provider credential from captured/persisted tool output. The active provider credential is removed from the command child environment so model-executed commands do not directly inherit it; however, shell startup files or commands may independently reintroduce credentials, so this does not provide OS-level isolation from parent-process inspection or transformed side channels. Bounds are required to prevent a hung or unbounded command from blocking the protocol or consuming the model context without limit.

## Invariants

- Lucy passes the shell command string without Lucy-side rewriting.
- Lucy uses SHELL when set and falls back to /bin/sh when SHELL is empty or unset.
- The configured provider API-key variable is removed from command children while the rest of the inherited environment is preserved.
- Lucy does not parse or sanitize shell startup behavior, which may independently reintroduce or retrieve credentials.
- Captured command output is redacted before protocol or session serialization.
- Command invocations retain the session cwd and shell-local cd does not mutate it.
- Timeout and output truncation produce a normalized tool result that the model can handle.
- After timeout or cancellation, Lucy terminates the shell process group and stops waiting for capture after a bounded grace period.
- Descendants that escape the process group or session are outside the v1 containment boundary, and incomplete capture is marked truncated.
- Foreground command output and exit status are persisted as part of the originating conversation turn.
- Background completion is persisted as a non-forgeable low-privilege observation and never as system or instruction context.
- Each provider adapter maps observations to its lowest available user-input privilege.
- Legacy background-completion system messages are downgraded during provider-message reconstruction without rewriting session files.
- A completed background command is delivered at the earliest provider-response boundary and starts an automatic turn if its originating turn already ended.
- Background commands survive user-turn cancellation, stop when Lucy exits, and are not reconstructed on session resume.
- JSONL one-shot mode remains alive after input EOF while registered background commands are running.

## Alternatives and trade-offs

A dedicated argv API would reduce shell interpretation but would not satisfy the intended command-line agent experience. A fixed `/bin/sh` would be more predictable, but would ignore the user's configured shell and startup behavior. Interactive process support would enable REPLs but requires stdin multiplexing and a broader lifecycle API. Poll, list, stop, and restart recovery for background commands remain deferred.

## Consequences

A background completion can no longer carry harness-level authority, so a model that treats delimited command output as an instruction is a model-side failure rather than a harness-granted privilege. Commands that exceed the timeout or output cap require the model to rerun them with narrower output or a shorter operation. Background completion can create an automatic provider request after control has returned to the user, but provider requests remain serialized. A trusted command can still inspect Lucy or exfiltrate transformed data outside the direct-output guarantee. The shell behavior is Unix-specific until a future platform decision is made.

## Enforcement

Integration tests MUST cover successful commands, non-zero exit codes, inherited cwd and ordinary environment variables, removal of the configured provider credential, timeout termination, stdout/stderr capture, output truncation, immediate background registration, and automatic completion delivery before and after an originating turn ends. Tests MUST verify that a timed-out shell/process group is terminated and that escaped-descendant capture returns within the grace bound.

## Revisit when

Reconsider this decision if callers need Windows, interactive stdin, durable background-job recovery, explicit job polling/stopping, persistent shell state, or stronger isolation than a trusted local shell.
