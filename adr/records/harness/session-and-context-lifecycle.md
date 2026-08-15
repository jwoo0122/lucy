---
id: harness.session-and-context-lifecycle
status: accepted
scope: harness
decision_type: lifecycle
applies_to:
- src/**
- tests/**
- README.md
summary: Lucy persists named JSONL sessions, previews their latest conversational messages, resumes their saved working directory when available, and preserves interruption, compaction, and boot-context state.
constrains: []
depends_on:
- harness.agent-boundary-and-protocol
- harness.configuration-and-provider
supersedes: []
superseded_by: []
last_reviewed: '2026-07-29'
enforcement:
- invariant: complete-session-records
  kind: executable
  check: rust-tests
- invariant: append-only-compaction-records
  kind: executable
  check: rust-tests
- invariant: latest-compaction-boundary
  kind: executable
  check: rust-tests
- invariant: ordered-interruption-records
  kind: executable
  check: rust-tests
- invariant: safe-incomplete-tool-recovery
  kind: executable
  check: rust-tests
- invariant: new-session-boot-snapshot
  kind: executable
  check: rust-tests
- invariant: historical-prompt-on-resume
  kind: executable
  check: rust-tests
- invariant: metadata-only-skill-catalog
  kind: executable
  check: rust-tests
- invariant: bounded-symlinked-skill-discovery
  kind: executable
  check: rust-tests
- invariant: immutable-skill-snapshot
  kind: executable
  check: rust-tests
- invariant: no-session-relationships
  kind: executable
  check: rust-tests
- invariant: immutable-cwd-with-observable-fallback
  kind: executable
  check: rust-tests
- invariant: conversational-session-previews
  kind: executable
  check: rust-tests
invariants:
- id: complete-session-records
  statement: Session records preserve the boot snapshot and all valid messages needed to reconstruct the active conversation.
- id: append-only-compaction-records
  statement: Compaction records are valid, append-only, secret-safe, complete-turn records that identify the summary, retained boundary, and token estimate without deleting history.
- id: latest-compaction-boundary
  statement: Resume and provider-message reconstruction apply only the latest compaction boundary and do not resend compacted-away raw messages.
- id: ordered-interruption-records
  statement: Interruption records are valid, append-only, secret-safe, ordered with surrounding messages, identify user cancellation, and replay in the TUI.
- id: safe-incomplete-tool-recovery
  statement: Incomplete tool-call fragments are never executed or sent to providers, and reconstructed cmd results close only a previously declared matching tool call.
- id: new-session-boot-snapshot
  statement: A new session records the current built-in composed prompt as boot_system_prompt.
- id: historical-prompt-on-resume
  statement: A resumed session sends its recorded boot_system_prompt even when the current binary would compose a different prompt.
- id: metadata-only-skill-catalog
  statement: A skill catalog entry does not claim to contain full skill instructions.
- id: bounded-symlinked-skill-discovery
  statement: Skill symlinks are followed only for expected target types, and cycles or duplicate resolved directories are not traversed repeatedly.
- id: immutable-skill-snapshot
  statement: Skill contents captured during discovery are persisted and used by explicit invocation, including when discovered through symlinks.
- id: no-session-relationships
  statement: Lucy does not infer or persist relationships between sessions.
- id: immutable-cwd-with-observable-fallback
  statement: The session header cwd remains immutable, while an unavailable saved cwd falls back for the invocation and is reported to interactive and machine clients.
- id: conversational-session-previews
  statement: Session-list previews use the latest user and assistant messages rather than trailing tool records.
---

# Session and boot context lifecycle

## Decision question

How should Lucy preserve chat history, interrupted turns, and ambient instructions across process restarts?

## Current decision

Lucy MUST store sessions as append-only JSONL files under `~/.lucy/sessions/<session-id>.jsonl`. A run without a session ID creates a new session; `--session <id>` resumes an existing session and MUST fail when the ID does not exist. `--list-sessions` MUST expose enough metadata to find resumable sessions.

Session metadata MUST expose the saved cwd and the latest user and assistant message summaries independently; tool messages MUST NOT replace those conversational previews. Resume MUST use the cwd saved in the immutable session header when it remains an accessible directory. If that cwd is unavailable, resume MUST use the invocation cwd, surface both saved and effective paths plus the fallback status, and MUST NOT rewrite the saved header cwd.

A session MAY contain valid JSONL interruption records. An interruption record MUST preserve the safe assistant output, tool-call/result observations, cancellation phase, and user-cancellation reason that were available at the nearest safe stopping point. Complete provider messages and completed/canceled tool results remain ordinary message records when their provider ordering is valid. If a canceled tool result could not be written as an ordinary message after its assistant tool call was persisted, a safe `cmd` interruption observation MAY be reconstructed as the matching provider tool message on the next request. Incomplete provider tool-call fragments MUST NOT be executed or sent as a malformed provider message. TUI replay MUST preserve the stored record order and show the interruption explicitly.

At new-session boot, Lucy MUST compose and snapshot the current compiled built-in system prompt, discovered instruction files, and available-skill catalog as `boot_system_prompt`. Resume MUST restore the exact historical `boot_system_prompt` from the session rather than recomposing it from the current binary or rereading current files. Built-in prompt changes and instruction-file changes therefore apply only to new sessions unless an explicit reload feature is added later.

Lucy MUST support append-only automatic compaction records without rewriting or deleting the earlier session history. When estimated context reaches at least 95% of the model window at a safe provider/cmd boundary, Lucy MUST use the configured model in a no-tools summary request, retain the most recent complete turns up to approximately 20,000 estimated tokens, and append a compaction record containing the summary, the retained-message boundary, and the pre-compaction token estimate. The active provider context after that boundary MUST be reconstructed as the boot system prompt, the compaction summary, and all retained/subsequent complete messages. Resume MUST apply the same latest compaction boundary. If summary generation or persistence fails, no compaction record or replacement boundary is appended; an ordinary user cancellation may still append the existing interruption record.

Session identity is caller-owned. Lucy MUST keep sessions independently replayable and MUST NOT persist parent-session links, child-session kinds, worker lifecycle state, or synthetic cross-session result delivery. A caller MAY launch several Lucy processes and send messages to each named session independently.

Instruction discovery MUST include `$XDG_CONFIG_HOME/lucy/AGENTS.md` or `$XDG_CONFIG_HOME/lucy/CLAUDE.md` as the global source (falling back to `~/.config/lucy` when `XDG_CONFIG_HOME` is unset or empty) and `AGENTS.md`/`CLAUDE.md` along the path from Git root to cwd. For one directory, `AGENTS.md` takes precedence over `CLAUDE.md`. Files are merged from broadest to most specific. A final `AGENTS.md` or `CLAUDE.md` symlink MUST be followed when it resolves to a regular file, including a target outside the instruction directory; symlinked intermediate instruction directories MUST still be ignored.

Skills MUST be discovered from the standard `.agents/skills/<name>/SKILL.md` locations globally and along the project path. The `.agents/skills` root, directories below it, and `SKILL.md` files MAY be symlinks, including links to targets outside the source tree, and MUST be followed when they resolve to directories or regular files as appropriate. Discovery MUST terminate safely when links form a directory cycle and MUST NOT traverse the same resolved directory more than once per skill root. The boot prompt MUST include skill name, description, and the logical path through which it was discovered, but not full skill contents. Explicit invocation uses the immutable contents captured during discovery.

## Context and forces

Chat usability requires state beyond one request. Reproducible resume requires preserving the model-visible boot context, while rereading mutable files on resume would silently change the meaning of an old conversation. Standard AGENTS/CLAUDE and Agent Skills locations provide interoperability without Lucy-specific resource trees. Following skill symlinks supports centrally managed skill collections while cycle detection keeps recursive discovery bounded. Separate process invocations can resume the same session through the existing append-only file.

## Invariants

- Session records preserve the boot snapshot and all valid messages needed to reconstruct the active conversation.
- Compaction records are valid, append-only, secret-safe, complete-turn records that identify the summary, retained boundary, and token estimate without deleting history.
- Resume and provider-message reconstruction apply only the latest compaction boundary and do not resend compacted-away raw messages.
- Interruption records are valid, append-only, secret-safe, ordered with surrounding messages, identify user cancellation, and replay in the TUI.
- Incomplete tool-call fragments are never executed or sent to providers, and reconstructed cmd results close only a previously declared matching tool call.
- A new session records the current built-in composed prompt as boot_system_prompt.
- A resumed session sends its recorded boot_system_prompt even when the current binary would compose a different prompt.
- A skill catalog entry does not claim to contain full skill instructions.
- Skill symlinks are followed only for expected target types, and cycles or duplicate resolved directories are not traversed repeatedly.
- Skill contents captured during discovery are persisted and used by explicit invocation, including when discovered through symlinks.
- Lucy does not infer or persist relationships between sessions.
- The session header cwd remains immutable, while an unavailable saved cwd falls back for the invocation and is reported to interactive and machine clients.
- Session-list previews use the latest user and assistant messages rather than trailing tool records.

## Alternatives and trade-offs

Recomposing the prompt on resume would apply the latest binary guidance immediately but break prompt stability and resume reproducibility. Embedding every skill body in the boot prompt would waste context and diverge from progressive disclosure conventions. Ignoring skill symlinks would reduce traversal complexity but prevent standard skill locations from referencing centrally managed files. Lucy chooses metadata-only boot context plus immutable skill-content snapshots, including symlink targets. Keeping session identity outside Lucy's relationship model lets callers orchestrate independent agents without adding a worker journal or scheduler.

## Consequences

Users must start a new session to pick up a newer built-in prompt or edited ambient instructions. A resumed session can report stale skill paths if the workspace moved or files were deleted; the resulting command error remains visible to the model. Concurrent writers to one session are not coordinated. Session files remain the source of truth for independent process invocations. A moved or deleted workspace permits resume from the caller's invocation directory, but callers must inspect the reported cwd fallback before assuming project identity.

## Enforcement

Tests MUST create, persist, close, and resume a session in a separate process; verify saved-cwd restoration, observable fallback to the invocation cwd without header mutation, and latest user/assistant previews that ignore trailing tool records; assert that new sessions snapshot the current built-in composed prompt and that resume retains a deliberately different historical `boot_system_prompt`; assert that the original boot snapshot is used after source-file edits; verify AGENTS/CLAUDE precedence, final instruction-file symlinks, intermediate-directory exclusion, ordinary and symlinked skill catalog discovery, and skill-directory cycle termination; and verify interruption records, safe partial output, ordering, and resume replay. Compaction tests MUST verify append-only persistence, latest-boundary reconstruction, complete-turn retention, summary redaction, resume equivalence, and unchanged session state when compaction fails or is canceled. Tests MUST verify that no child-session or background-result records are created.

## Revisit when

Reconsider this decision if sessions need live built-in-prompt or instruction reload, branching/compaction, server-side conversation state, cross-process locking, or a dedicated event journal with stronger transactional guarantees.
