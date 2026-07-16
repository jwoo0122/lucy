---
id: harness.session-and-context-lifecycle
status: accepted
scope: harness
decision_type: lifecycle
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy persists named JSONL sessions and restores an immutable boot-context snapshot on resume.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
  - harness.configuration-and-provider
supersedes: []
superseded_by: []
last_reviewed: "2026-07-16"
---

# Session and boot context lifecycle

## Decision question

How should Lucy preserve chat history and ambient instructions across process restarts?

## Current decision

Lucy MUST store sessions as JSONL files under `~/.lucy/sessions/<session-id>.jsonl`. A run without a session ID creates a new session; `--session <id>` resumes an existing session and MUST fail when the ID does not exist. `--list-sessions` MUST expose enough metadata to find resumable sessions.

At new-session boot, Lucy MUST resolve and snapshot the configured system prompt, discovered instruction files, and available-skill catalog. Resume MUST restore that exact snapshot rather than rereading current files. Changes to config or instruction files therefore apply only to new sessions unless an explicit reload feature is added later.

Instruction discovery MUST include `~/.lucy/AGENTS.md` or `~/.lucy/CLAUDE.md` as the global source and `AGENTS.md`/`CLAUDE.md` along the path from Git root to cwd. For one directory, `AGENTS.md` takes precedence over `CLAUDE.md`. Files are merged from broadest to most specific. Symlinked instruction files MUST be ignored rather than followed.

Skills MUST be discovered only from the standard `.agents/skills/<name>/SKILL.md` directories globally and along the project path. Symlinked skill directories and `SKILL.md` files MUST be ignored rather than followed. The boot prompt MUST include skill name, description, and path, but not full skill contents. The model loads a relevant skill through `cmd` when needed.

## Context and forces

Chat usability requires state beyond one request. Reproducible resume requires preserving the model-visible boot context, while rereading mutable files on resume would silently change the meaning of an old conversation. Standard AGENTS/CLAUDE and Agent Skills locations provide interoperability without Lucy-specific resource trees.

## Invariants

- Session records include the boot snapshot and all user, assistant, tool-call, and tool-result messages needed to reconstruct the active conversation.
- Newly created session headers MUST reject cwd or provider-setting values containing the active provider key.
- A resumed session whose current provider key is already present in the raw file MUST be rejected and omitted from listing rather than sent or summarized; every decoded JSON value is scanned before typed deserialization, including unknown and nested fields.
- Session appends MUST open the final path component without following symlinks, then verify the opened descriptor is a regular owner-only file before writing.
- A resumed session sends the same boot snapshot that was recorded at session creation.
- `AGENTS.md` wins over `CLAUDE.md` in the same directory; more specific directories are appended later.
- A skill catalog entry never claims to contain the full skill instructions.
- Skill file contents loaded through `cmd` become ordinary tool results and are eligible for session persistence.

## Alternatives and trade-offs

Rereading context on every turn would observe edits immediately but break prompt stability and resume reproducibility. Embedding every skill file would simplify skill loading but waste context and diverge from progressive disclosure conventions. Lucy chooses snapshots plus command-based loading.

## Consequences

Users must start a new session to pick up edited ambient instructions. A resumed session can report stale skill paths if the workspace moved or files were deleted; the resulting command error remains visible to the model. Credential rotation does not migrate old-key session data. Cross-process session locking and crash-recovery rewriting are outside this minimal v1 lifecycle.

## Enforcement

Integration tests MUST create, persist, close, and resume a session; assert that the original boot snapshot is used after source-file edits; verify AGENTS/CLAUDE precedence; and verify skill catalog discovery and duplicate-name precedence.

## Revisit when

Reconsider this decision if sessions need live instruction reload, branching/compaction, server-side conversation state, or a dedicated skill-loading tool.
