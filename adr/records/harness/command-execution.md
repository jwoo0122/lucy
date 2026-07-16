---
id: harness.command-execution
status: accepted
scope: harness
decision_type: execution
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy executes trusted finite shell commands locally with bounded time and output.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
  - harness.session-and-context-lifecycle
supersedes: []
superseded_by: []
last_reviewed: "2026-07-16"
---

# Trusted local command execution

## Decision question

What execution semantics does the v1 `cmd` tool provide?

## Current decision

Lucy MUST target macOS/Linux in v1 and execute `cmd` arguments through `/bin/sh -lc`. The command MUST run from the session's starting cwd and inherit the Lucy process environment except for the configured provider API-key environment variable, which MUST be removed before spawning the shell. stdin MUST be disconnected. Lucy MUST support finite commands only; interactive, daemon, and long-lived process management are out of scope.

Each command MUST have a 10-minute timeout. stdout and stderr MUST each be bounded to 64 KiB; truncation MUST be represented in the normalized tool result. A non-zero exit is a successful tool invocation with its exit code and captured output, not a harness-level protocol error.

## Context and forces

The model is explicitly trusted and the harness is local-only, so v1 does not add approval, sandboxing, or an allowlist. Lucy removes the provider credential from the direct child environment and redacts it from captured/persisted tool output. This does not provide OS-level isolation from parent-process inspection or transformed side channels. Bounds are required to prevent a hung or unbounded command from blocking the single-turn protocol or consuming the model context without limit.

## Invariants

- The shell command string is passed without Lucy-side rewriting.
- The configured provider API-key variable is absent from the child environment.
- Captured command output is redacted before protocol or session serialization.
- The command cwd is stable across invocations; shell-local `cd` does not mutate the session cwd.
- Timeout and output truncation produce a tool result that the model can handle.
- After timeout or cancellation, Lucy terminates the shell's process group and stops waiting for capture after a bounded grace period.
- Descendants that deliberately escape the process group/session are outside the v1 containment boundary and may continue; any incomplete capture is marked truncated.
- Command output and exit status are persisted as part of the conversation turn.

## Alternatives and trade-offs

A dedicated argv API would reduce shell interpretation but would not satisfy the intended command-line agent experience. Interactive process support would enable servers and REPLs but requires stdin multiplexing, cancellation, and process lifecycle APIs. Lucy defers that complexity.

## Consequences

Commands that exceed the timeout or output cap require the model to rerun them with narrower output or a shorter operation. A trusted command can still inspect Lucy or exfiltrate transformed data outside the direct-output guarantee. The shell behavior is Unix-specific until a future platform decision is made.

## Enforcement

Integration tests MUST cover successful commands, non-zero exit codes, inherited cwd, timeout termination, stdout/stderr capture, and output truncation. Tests MUST verify that a timed-out shell/process group is terminated and that escaped-descendant capture returns within the grace bound.

## Revisit when

Reconsider this decision if callers need Windows, interactive stdin, background processes, persistent shell state, or stronger isolation than a trusted local shell.
