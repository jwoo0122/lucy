---
id: harness.session-and-context-lifecycle
status: accepted
scope: harness
decision_type: lifecycle
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy persists named JSONL sessions, interruption records, compaction boundaries, and the built-in composed boot prompt snapshot that was current when each session was created.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
  - harness.configuration-and-provider
supersedes: []
superseded_by: []
last_reviewed: "2026-07-26"
enforcement:
  - id: session-resume-and-context
    path: src/session.rs
    must_contain:
      - "fn creates_appends_resumes_and_lists_jsonl_session()"
      - "fn resume_retains_historical_boot_system_prompt()"
      - "fn compaction_appends_a_boundary_and_reconstructs_only_retained_messages()"
      - "fn interruption_records_are_valid_and_resume_in_file_order_without_provider_fragments()"
    must_not_contain: []
enforcement_exception: null
---

# Session and boot context lifecycle

## Decision question

How should Lucy preserve chat history, interrupted turns, and ambient instructions across process restarts?

## Current decision

Lucy MUST store sessions as append-only JSONL files under `~/.lucy/sessions/<session-id>.jsonl`. A run without a session ID creates a new session; `--session <id>` resumes an existing session and MUST fail when the ID does not exist. `--list-sessions` MUST expose enough metadata to find resumable sessions.

A session MAY contain valid JSONL interruption records. An interruption record MUST preserve the safe assistant output, tool-call/result observations, cancellation phase, and user-cancellation reason that were available at the nearest safe stopping point. Complete provider messages and completed/canceled tool results remain ordinary message records when their provider ordering is valid. If a canceled tool result could not be written as an ordinary message after its assistant tool call was persisted, a safe `cmd` interruption observation MAY be reconstructed as the matching provider tool message on the next request. Incomplete provider tool-call fragments MUST NOT be executed or sent as a malformed provider message. TUI replay MUST preserve the stored record order and show the interruption explicitly.

At new-session boot, Lucy MUST compose and snapshot the current compiled built-in system prompt, discovered instruction files, and available-skill catalog as `boot_system_prompt`. Resume MUST restore the exact historical `boot_system_prompt` from the session rather than recomposing it from the current binary or rereading current files. Built-in prompt changes and instruction-file changes therefore apply only to new sessions unless an explicit reload feature is added later.

Lucy MUST support append-only automatic compaction records without rewriting or deleting the earlier session history. When estimated context reaches at least 95% of the model window at a safe provider/cmd boundary, Lucy MUST use the configured model in a no-tools summary request, retain the most recent complete turns up to approximately 20,000 estimated tokens, and append a compaction record containing the summary, the retained-message boundary, and the pre-compaction token estimate. The active provider context after that boundary MUST be reconstructed as the boot system prompt, the compaction summary, and all retained/subsequent complete messages. Resume MUST apply the same latest compaction boundary. If summary generation or persistence fails, no compaction record or replacement boundary is appended; an ordinary user cancellation may still append the existing interruption record.

Session identity is caller-owned. Lucy MUST keep sessions independently replayable and MUST NOT persist parent-session links, child-session kinds, worker lifecycle state, or synthetic cross-session result delivery. A caller MAY launch several Lucy processes and send messages to each named session independently.

Instruction discovery MUST include `$XDG_CONFIG_HOME/lucy/AGENTS.md` or `$XDG_CONFIG_HOME/lucy/CLAUDE.md` as the global source (falling back to `~/.config/lucy` when `XDG_CONFIG_HOME` is unset or empty) and `AGENTS.md`/`CLAUDE.md` along the path from Git root to cwd. For one directory, `AGENTS.md` takes precedence over `CLAUDE.md`. Files are merged from broadest to most specific. A final `AGENTS.md` or `CLAUDE.md` symlink MUST be followed when it resolves to a regular file, including a target outside the instruction directory; symlinked intermediate instruction directories MUST still be ignored.

Skills MUST be discovered only from the standard `.agents/skills/<name>/SKILL.md` directories globally and along the project path. Symlinked skill directories and `SKILL.md` files MUST be ignored rather than followed. The boot prompt MUST include skill name, description, and path, but not full skill contents. The model loads a relevant skill through `cmd` when needed.

## Context and forces

Chat usability requires state beyond one request. Reproducible resume requires preserving the model-visible boot context, while rereading mutable files on resume would silently change the meaning of an old conversation. Standard AGENTS/CLAUDE and Agent Skills locations provide interoperability without Lucy-specific resource trees. Separate process invocations can resume the same session through the existing append-only file.

## Invariants

- Session records include the boot snapshot and all valid user, assistant, tool-call, and tool-result messages needed to reconstruct the active conversation.
- Compaction records are valid JSONL, append-only, secret-safe, ordered at a complete turn boundary, and identify the summary, retained-message boundary, and token estimate. Historical messages remain available for replay even when they are omitted from the next provider context.
- Resume and `provider_messages()` apply only the latest compaction boundary on the active session path; they do not send compacted-away raw messages in addition to the summary.
- Interruption records are valid JSONL, append-only, secret-safe, ordered with surrounding messages, and explicitly identify user cancellation; they are replayed by the TUI.
- Incomplete provider tool-call fragments are retained only as safe interruption observations and are never executed or included in provider message history; safe `cmd` result observations may only close a previously declared matching tool call.
- A new session records the current built-in composed prompt as `boot_system_prompt`.
- A resumed session sends its recorded historical `boot_system_prompt`, even when the current binary would compose a different prompt for a new session.
- A skill catalog entry never claims to contain the full skill instructions.
- Skill file contents loaded through `cmd` become ordinary tool results and are eligible for session persistence.
- Lucy does not infer or persist relationships between sessions.

## Alternatives and trade-offs

Recomposing the prompt on resume would apply the latest binary guidance immediately but break prompt stability and resume reproducibility. Embedding every skill file would simplify skill loading but waste context and diverge from progressive disclosure conventions. Lucy chooses snapshots plus command-based loading. Keeping session identity outside Lucy's relationship model lets callers orchestrate independent agents without adding a worker journal or scheduler.

## Consequences

Users must start a new session to pick up a newer built-in prompt or edited ambient instructions. A resumed session can report stale skill paths if the workspace moved or files were deleted; the resulting command error remains visible to the model. Concurrent writers to one session are not coordinated. Session files remain the source of truth for independent process invocations.

## Enforcement

Tests MUST create, persist, close, and resume a session in a separate process; assert that new sessions snapshot the current built-in composed prompt and that resume retains a deliberately different historical `boot_system_prompt`; assert that the original boot snapshot is used after source-file edits; verify AGENTS/CLAUDE precedence, final instruction-file symlinks, intermediate-directory exclusion, and skill catalog discovery; and verify interruption records, safe partial output, ordering, and resume replay. Compaction tests MUST verify append-only persistence, latest-boundary reconstruction, complete-turn retention, summary redaction, resume equivalence, and unchanged session state when compaction fails or is canceled. Tests MUST verify that no child-session or background-result records are created.

## Revisit when

Reconsider this decision if sessions need live built-in-prompt or instruction reload, branching/compaction, server-side conversation state, cross-process locking, or a dedicated event journal with stronger transactional guarantees.
