---
id: harness.session-and-context-lifecycle
status: accepted
scope: harness
decision_type: lifecycle
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy persists named JSONL sessions, interruption records, and an immutable boot-context snapshot on resume.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
  - harness.configuration-and-provider
supersedes: []
superseded_by: []
last_reviewed: "2026-07-19"
---

# Session and boot context lifecycle

## Decision question

How should Lucy preserve chat history, interrupted turns, and ambient instructions across process restarts?

## Current decision

Lucy MUST store sessions as append-only JSONL files under `~/.lucy/sessions/<session-id>.jsonl`. A run without a session ID creates a new session; `--session <id>` resumes an existing session and MUST fail when the ID does not exist. `--list-sessions` MUST expose enough metadata to find resumable sessions.

In addition to the existing session header and provider messages, a session MAY contain valid JSONL interruption records. An interruption record MUST preserve the safe assistant output, tool-call/result observations, cancellation phase, and user-cancellation reason that were available at the nearest safe stopping point. Complete provider messages and completed/canceled tool results remain ordinary message records when their provider ordering is valid. If a canceled tool result could not be written as an ordinary message after its assistant tool call was persisted, a safe `cmd` interruption observation MAY be reconstructed as the matching provider tool message on the next request. Incomplete provider tool-call fragments MUST NOT be executed or sent as a malformed provider message. TUI replay MUST preserve the stored record order and show the interruption explicitly.

At new-session boot, Lucy MUST resolve and snapshot the configured system prompt, discovered instruction files, and available-skill catalog. Resume MUST restore that exact snapshot rather than rereading current files. Changes to config or instruction files therefore apply only to new sessions unless an explicit reload feature is added later.

Lucy MUST support append-only automatic compaction records without rewriting or deleting the earlier session history. When estimated context reaches at least 95% of the model window at a safe provider/cmd boundary, Lucy MUST use the configured model in a no-tools summary request, retain the most recent complete turns up to approximately 20,000 estimated tokens, and append a compaction record containing the summary, the retained-message boundary, and the pre-compaction token estimate. The active provider context after that boundary MUST be reconstructed as the boot system prompt, the compaction summary, and all retained/subsequent complete messages. Resume MUST apply the same latest compaction boundary. If summary generation or persistence fails, no compaction record or replacement boundary is appended; an ordinary user cancellation may still append the existing interruption record.

Child subagent sessions MUST use the same append-only JSONL storage and secret-safety rules as main sessions, with a separate file and a `parent_session_id` link in the child header. A child header MUST identify its `session_kind`, delegated task, cwd, and selected provider settings. Child records MUST retain the full child transcript, lifecycle transitions, terminal result/error, and interruption reason. The parent session MUST separately retain typed append-only background-result records: one pending record owns the completion payload and one optional delivered record references its completion ID and delivery position in the logical main turn. Provider-specific synthetic assistant/tool messages are reconstructed from these records and MUST NOT replace them as the persistence source of truth. A child process-shutdown interruption MUST be recorded as `interrupted` with reason `process_shutdown`; the first implementation MUST NOT resume a running child after a Lucy restart.

Instruction discovery MUST include `$XDG_CONFIG_HOME/lucy/AGENTS.md` or `$XDG_CONFIG_HOME/lucy/CLAUDE.md` as the global source (falling back to `~/.config/lucy` when `XDG_CONFIG_HOME` is unset or empty) and `AGENTS.md`/`CLAUDE.md` along the path from Git root to cwd. For one directory, `AGENTS.md` takes precedence over `CLAUDE.md`. Files are merged from broadest to most specific. A final `AGENTS.md` or `CLAUDE.md` symlink MUST be followed when it resolves to a regular file, including a target outside the instruction directory; symlinked intermediate instruction directories MUST still be ignored.

Skills MUST be discovered only from the standard `.agents/skills/<name>/SKILL.md` directories globally and along the project path. Symlinked skill directories and `SKILL.md` files MUST be ignored rather than followed. The boot prompt MUST include skill name, description, and path, but not full skill contents. The model loads a relevant skill through `cmd` when needed.

## Context and forces

Chat usability requires state beyond one request. Reproducible resume requires preserving the model-visible boot context, while rereading mutable files on resume would silently change the meaning of an old conversation. Standard AGENTS/CLAUDE and Agent Skills locations provide interoperability without Lucy-specific resource trees. The global instruction file shares Lucy's XDG configuration directory so user-owned configuration and global guidance move together.

## Invariants

- Session records include the boot snapshot and all valid user, assistant, tool-call, and tool-result messages needed to reconstruct the active conversation.
- Child session records include a parent-session link, immutable boot snapshot, delegated task, full valid transcript, and append-only lifecycle status records; child and parent files remain independently replayable.
- Parent background-result records are append-only, secret-safe, preserve completion order and identity, contain at most one delivered transition per completion ID, and reconstruct a delivered synthetic tool observation at its exact main-context position without exposing the result as user input.
- Compaction records are valid JSONL, append-only, secret-safe, ordered at a complete turn boundary, and identify the summary, retained-message boundary, and token estimate. Historical messages remain available for replay even when they are omitted from the next provider context.
- Resume and `provider_messages()` apply only the latest compaction boundary on the active session path; they do not send compacted-away raw messages in addition to the summary.
- Interruption records are valid JSONL, append-only, secret-safe, ordered with surrounding messages, and explicitly identify user cancellation; they are replayed by the TUI.
- Incomplete provider tool-call fragments are retained only as safe interruption observations and are never executed or included in provider message history; safe `cmd` result observations may only close a previously declared matching tool call.
- Newly created session headers MUST reject cwd or provider-setting values containing the active provider key.
- A resumed session whose current provider key is already present in the raw file MUST be rejected and omitted from listing rather than sent or summarized; every decoded JSON value is scanned before typed deserialization, including unknown and nested fields.
- Session appends MUST open the final path component without following symlinks, then verify the opened descriptor is a regular owner-only file before writing.
- A resumed session sends the same boot snapshot that was recorded at session creation.
- `AGENTS.md` wins over `CLAUDE.md` in the same directory; the XDG Lucy config directory is the broadest global source and more specific project directories are appended later. Final instruction-file symlinks are read only through an opened regular-file descriptor, while intermediate directory symlinks remain excluded.
- A skill catalog entry never claims to contain the full skill instructions.
- Skill file contents loaded through `cmd` become ordinary tool results and are eligible for session persistence.

## Alternatives and trade-offs

Rereading context on every turn would observe edits immediately but break prompt stability and resume reproducibility. Embedding every skill file would simplify skill loading but waste context and diverge from progressive disclosure conventions. Lucy chooses snapshots plus command-based loading.

## Consequences

Users must start a new session to pick up edited ambient instructions. A resumed session can report stale skill paths if the workspace moved or files were deleted; the resulting command error remains visible to the model. Credential rotation does not migrate old-key session data; legacy data containing an old inactive key remains a user-managed residual. Partial canceled output is faithfully recoverable in the transcript without forcing malformed provider tool history. Compaction can reduce the active provider context while retaining the full append-only history, but the generated summary is a model-derived representation and is not a byte-for-byte substitute for omitted messages. Cross-process session locking and crash-recovery rewriting are outside this minimal lifecycle.

## Enforcement

Tests MUST create, persist, close, and resume a session; assert that the original boot snapshot is used after source-file edits; verify AGENTS/CLAUDE precedence, final instruction-file symlinks, intermediate-directory exclusion, and skill catalog discovery; and verify interruption records, safe partial output, ordering, and resume replay. They MUST verify pending/delivered background-result persistence, exactly-once reconstruction as synthetic tool observations, secret rejection, and undelivered-result discovery after resume. Compaction tests MUST verify append-only persistence, latest-boundary reconstruction, complete-turn retention, summary redaction, resume equivalence, and unchanged session state when compaction fails or is canceled.

## Revisit when

Reconsider this decision if sessions need live instruction reload, branching/compaction, server-side conversation state, or a dedicated event journal with stronger transactional guarantees.
