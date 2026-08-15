---
id: harness.session-and-context-lifecycle
status: accepted
scope: harness
decision_type: lifecycle
applies_to:
- src/**
- tests/**
- README.md
summary: Lucy persists named JSONL sessions with a fail-fast OS-backed single-writer lease, while normal provider context pressure is handled by deterministic projection and legacy compaction records remain readable compatibility state.
constrains: []
depends_on:
- harness.agent-boundary-and-protocol
- harness.configuration-and-provider
supersedes: []
superseded_by: []
last_reviewed: '2026-08-15'
enforcement:
- invariant: complete-session-records
  kind: executable
  check: rust-tests
- invariant: fail-fast-single-writer-lease
  kind: executable
  check: rust-tests
- invariant: deterministic-provider-projection
  kind: executable
  check: rust-tests
- invariant: legacy-compaction-read-compatibility
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
- invariant: active-turn-steering-order
  kind: executable
  check: rust-tests
invariants:
- id: complete-session-records
  statement: Session records preserve the boot snapshot and all valid messages needed to reconstruct the stored conversation without rewriting raw history for context pressure.
- id: fail-fast-single-writer-lease
  statement: A mutable session handle holds an OS-backed exclusive writer lease for its lifetime; a competing same-session mutable open fails without waiting and succeeds after release.
- id: deterministic-provider-projection
  statement: When Lucy has trusted context-window metadata, normal provider requests derive a bounded deterministic view from stored messages without an LLM-authored memory summary, preserve the current request, retain a structurally valid recent suffix, and leave persisted history unchanged.
- id: legacy-compaction-read-compatibility
  statement: Existing persisted compaction records remain append-only compatibility input and resume applies only the latest valid boundary, but normal frontend execution does not create new semantic compaction records as its context-pressure policy.
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
- id: active-turn-steering-order
  statement: User messages accepted while a TUI turn is active are appended to that active turn in submission order at the next safe provider boundary, and only messages still pending may be recalled for editing.
---

# Session and boot context lifecycle

## Decision question

How should Lucy preserve chat history, interrupted turns, and ambient instructions across process restarts while keeping provider context bounded?

## Current decision

Lucy MUST store sessions as append-only JSONL files under `~/.lucy/sessions/<session-id>.jsonl`. A run without a session ID creates a new session; `--session <id>` resumes an existing session and MUST fail when the ID does not exist. `--list-sessions` MUST expose enough metadata to find resumable sessions.

Session metadata MUST expose the saved cwd and the latest user and assistant message summaries independently; tool messages MUST NOT replace those conversational previews. Resume MUST use the cwd saved in the immutable session header when it remains an accessible directory. If that cwd is unavailable, resume MUST use the invocation cwd, surface both saved and effective paths plus the fallback status, and MUST NOT rewrite the saved header cwd.

A mutable session handle MUST hold a per-session OS-backed exclusive writer lease for its lifetime. While that lease is held, another mutable create or resume of the same session MUST fail without waiting; after the handle releases the lease, a later mutable open MUST succeed. This is the full concurrency guarantee enforced by repository tests. They do not establish lock fairness, waiting or queueing semantics, cross-host or network-filesystem exclusion, or crash-recovery behavior beyond the exercised release path.

While a TUI turn is active, additional user submissions MUST remain visibly pending until accepted at a safe provider boundary. Accepted steering messages MUST be appended to the same active turn in submission order and included in the next provider request; they MUST NOT open a second concurrent turn. The TUI MAY recall only a message that remains pending, and recall MUST remove it from delivery before restoring it to the input editor. A submission racing with turn completion MUST be delivered as steering when accepted before completion or as the next turn otherwise; it MUST NOT remain stranded in the visible queue.

A session MAY contain valid JSONL interruption records. An interruption record MUST preserve the safe assistant output, tool-call/result observations, cancellation phase, and user-cancellation reason that were available at the nearest safe stopping point. Complete provider messages and completed/canceled tool results remain ordinary message records when their provider ordering is valid. If a canceled tool result could not be written as an ordinary message after its assistant tool call was persisted, a safe `cmd` interruption observation MAY be reconstructed as the matching provider tool message on the next request. Incomplete provider tool-call fragments MUST NOT be executed or sent as a malformed provider message. TUI replay MUST preserve the stored record order and show the interruption explicitly.

At new-session boot, Lucy MUST compose and snapshot the current compiled built-in system prompt, discovered instruction files, and available-skill catalog as `boot_system_prompt`. Resume MUST restore the exact historical `boot_system_prompt` from the session rather than recomposing it from the current binary or rereading current files. Built-in prompt changes and instruction-file changes therefore apply only to new sessions unless an explicit reload feature is added later.

Normal execution MUST NOT use an LLM-authored semantic summary as its default response to context pressure. When Lucy has trusted context-window metadata for the configured provider, the provider-facing message list MUST be derived deterministically from the stored conversation. Projection MUST preserve the system message, MUST preserve the current user or observation request as an anchor, MUST keep the earliest structurally safe recent suffix that fits the usable token budget, and MUST NOT emit an orphan tool result or a declared tool call without its result. Lucy MAY first replace sufficiently old large tool outputs with deterministic bounded placeholders whose exact source remains in persisted history. When earlier messages are omitted from the active provider view, any breadcrumb MUST state only factual omission information and MUST NOT synthesize topics, lessons, importance, intent, or narrative memory. Projection MUST NOT rewrite or delete session history.

Lucy MUST NOT invent a context size for an arbitrary OpenAI-compatible endpoint. Automatic projection metadata MAY be obtained only from provider surfaces Lucy explicitly trusts for this purpose, currently Codex-owned metadata and OpenRouter's model catalog. If trusted metadata is unavailable, Lucy sends the structurally valid stored conversation without proactive size projection and lets the provider report any context limit rather than guessing one.

A session MAY contain compaction records written by older Lucy versions. Such records remain valid append-only compatibility state: resume and provider-message reconstruction MAY apply the latest valid compaction boundary so an existing historical session remains replayable according to the representation it already stored. Normal frontend execution MUST NOT create new semantic compaction records as its context-pressure mechanism. Compatibility parsing of old records does not make their summaries the memory model for newly projected execution.

Session identity is caller-owned. Lucy MUST keep sessions independently replayable and MUST NOT persist parent-session links, child-session kinds, worker lifecycle state, or synthetic cross-session result delivery. A caller MAY launch several Lucy processes and send messages to each named session independently.

Instruction discovery MUST include `$XDG_CONFIG_HOME/lucy/AGENTS.md` or `$XDG_CONFIG_HOME/lucy/CLAUDE.md` as the global source (falling back to `~/.config/lucy` when `XDG_CONFIG_HOME` is unset or empty) and `AGENTS.md`/`CLAUDE.md` along the path from Git root to cwd. For one directory, `AGENTS.md` takes precedence over `CLAUDE.md`. Files are merged from broadest to most specific. A final `AGENTS.md` or `CLAUDE.md` symlink MUST be followed when it resolves to a regular file, including a target outside the instruction directory; symlinked intermediate instruction directories MUST still be ignored.

Skills MUST be discovered from the standard `.agents/skills/<name>/SKILL.md` locations globally and along the project path. The `.agents/skills` root, directories below it, and `SKILL.md` files MAY be symlinks, including links to targets outside the source tree, and MUST be followed when they resolve to directories or regular files as appropriate. Discovery MUST terminate safely when links form a directory cycle and MUST NOT traverse the same resolved directory more than once per skill root. The boot prompt MUST include skill name, description, and the logical path through which it was discovered, but not full skill contents. Explicit invocation uses the immutable contents captured during discovery.

## Context and forces

Chat usability requires state beyond one request. Reproducible resume currently requires preserving the model-visible boot context, while rereading mutable files on resume would silently change the meaning of an old conversation. At the same time, an LLM-authored compaction summary makes a past model or harness interpretation part of future attention and can irreversibly suppress details that a stronger future model would have used differently. Deterministic projection keeps the stored record intact and treats the provider window as disposable attention rather than canonical memory.

Standard AGENTS/CLAUDE and Agent Skills locations provide interoperability without Lucy-specific resource trees. Following skill symlinks supports centrally managed skill collections while cycle detection keeps recursive discovery bounded. Separate process invocations can resume the same session through the existing append-only file. The OS-backed lease prevents the tested overlapping mutable access from silently appending concurrently and rejects that overlap immediately.

## Invariants

- Session records preserve the boot snapshot and all valid messages needed to reconstruct the stored conversation without rewriting raw history for context pressure.
- A mutable session handle holds an OS-backed exclusive writer lease for its lifetime; a competing same-session mutable open fails without waiting and succeeds after release.
- When trusted context metadata exists, normal provider requests use a deterministic bounded projection rather than an LLM-authored semantic compaction summary.
- Projection preserves the current request, retains a structurally valid recent suffix, leaves persisted history unchanged, and uses only factual breadcrumbs for omitted ranges.
- Existing persisted compaction records remain readable append-only compatibility input and only the latest valid boundary is applied on historical resume.
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
- User messages accepted while a TUI turn is active are appended to that active turn in submission order at the next safe provider boundary, and only messages still pending may be recalled for editing.

## Alternatives and trade-offs

LLM-authored rolling summaries keep more interpreted history resident in the prompt, but they require a past model to predict what a future model will need and make a lossy semantic representation part of continuing context. Lucy chooses deterministic omission plus exact recoverability instead. Embeddings, topic maps, importance scores, and narrative memories may improve retrieval in some workloads, but making them required harness state would encode another semantic prior into the memory substrate; they are therefore outside this decision.

Inventing a conservative context window for unknown compatible providers would permit proactive projection everywhere but creates provider knowledge Lucy does not actually possess. Lucy instead projects automatically only when the provider exposes a trusted size and accepts provider-side overflow as the honest failure mode otherwise.

Recomposing the prompt on resume would apply the latest binary guidance immediately but break prompt stability and resume reproducibility. Embedding every skill body in the boot prompt would waste context and diverge from progressive disclosure conventions. Ignoring skill symlinks would reduce traversal complexity but prevent standard skill locations from referencing centrally managed files. Lucy chooses metadata-only boot context plus immutable skill-content snapshots, including symlink targets, while sessions remain the persistence authority.

## Consequences

Users must start a new session to pick up a newer built-in prompt or edited ambient instructions. A resumed session can report stale skill paths if the workspace moved or files were deleted; the resulting command error remains visible to the model. Historical sessions that already contain compaction records may still resume through their saved summary boundary, while newly projected execution does not create another semantic summary solely because context is full.

The provider facade consumes trusted context-window metadata internally for projection and does not feed that value through the legacy compaction-control field. Until the interactive status surface becomes projection-aware, the TUI may omit a context-window denominator rather than display a value that also re-enables semantic compaction.

A competing same-session mutable open fails immediately while the tested writer lease is held and succeeds after release. Callers that need waiting, fairness, cross-host exclusion, network-filesystem guarantees, or stronger crash recovery must provide those semantics themselves. Session files remain the source of truth for independent process invocations in this decision. A moved or deleted workspace permits resume from the caller's invocation directory, but callers must inspect the reported cwd fallback before assuming project identity.

## Enforcement

Tests MUST verify that direct lease acquisition fails without waiting while held, that a second mutable same-session handle is rejected, and that both paths succeed after release; create, persist, close, and resume a session in a separate process; verify saved-cwd restoration, observable fallback to the invocation cwd without header mutation, and latest user/assistant previews that ignore trailing tool records; assert that new sessions snapshot the current built-in composed prompt and that resume retains a deliberately different historical `boot_system_prompt`; assert that the original boot snapshot is used after source-file edits; verify AGENTS/CLAUDE precedence, final instruction-file symlinks, intermediate-directory exclusion, ordinary and symlinked skill catalog discovery, and skill-directory cycle termination; and verify interruption records, safe partial output, ordering, and resume replay.

Projection tests MUST verify identity behavior when context fits, deterministic recent-history selection when it does not, current-request anchoring, factual omission counts, and tool-call/result validity at retained boundaries. Provider tests MUST verify that automatic generic-provider metadata lookup is restricted to the explicitly trusted provider surface. Existing compaction tests MAY remain to verify historical record parsing, latest-boundary reconstruction, summary redaction, and resume compatibility, but those tests MUST NOT be interpreted as the normal context-pressure policy.

Tests MUST verify that no child-session or background-result records are created.

## Revisit when

Reconsider this decision when the append-only Lucy journal becomes the persistence authority instead of named sessions, or if sessions need live built-in-prompt or instruction reload, branching, server-side conversation state, waiting or fair lock acquisition, cross-host or network-filesystem coordination, or stronger stale-lease recovery.
