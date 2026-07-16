---
id: harness.configuration-and-provider
status: accepted
scope: harness
decision_type: configuration
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy bootstraps a user-editable ~/.lucy/config.toml and reads provider credentials only from the environment.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
supersedes: []
superseded_by: []
last_reviewed: "2026-07-16"
---

# User-owned configuration and provider boundary

## Decision question

Where do Lucy's minimal system prompt and LLM connection settings live, and when do they take effect?

## Current decision

Lucy MUST create `~/.lucy/config.toml` on first run when it does not exist, and MUST never overwrite an existing file during bootstrap or upgrade. The file MUST expose a user-editable `system_prompt` plus `[llm]` settings for `base_url`, `model`, and `api_key_env`.

The generated prompt MUST be minimal and editable:

- the model can access computer resources;
- it should use the provided tools to achieve the user's requirements;
- it should read a relevant skill's `SKILL.md` with `cmd` when needed.

The generated config SHOULD use OpenRouter's OpenAI-compatible endpoint as its example/default base URL, while all compatible endpoints remain configurable. The generated model value MUST be empty so Lucy does not guess a time-sensitive provider model; starting a session without a model MUST fail with a clear configuration error. API credentials MUST be read from the configured environment variable and MUST NOT be stored in config, session files, protocol events, or diagnostics. A credential containing JSON syntax/control characters, only decimal digits, or a complete fixed protocol/storage literal MUST be rejected before it can enter serialized output; these values cannot be safely redacted while preserving the schema. Newly created session headers MUST also reject any cwd or LLM setting containing the active credential. The generated OpenRouter example uses `OPENROUTER_API_KEY`; the runtime default credential variable is `OPENAI_API_KEY` when `api_key_env` is omitted.

Lucy MUST resolve config and ambient context at new-session boot and persist the resolved system prompt in the session snapshot. Existing sessions MUST NOT change when the config file is edited.

## Context and forces

Users need to inspect and change the minimal model guidance without recompiling Lucy. Cargo installation has no portable user-home post-install hook, so first-run bootstrap is the reliable installation-independent behavior. Credentials are secrets and should not enter durable user-controlled artifacts or the direct command environment. Command execution remains useful through the rest of the inherited process environment, without granting the shell the provider credential directly. This is not OS-level process isolation: parent-process inspection and transformed side channels remain outside the v1 guarantee.

## Invariants

- Missing config is created once with safe parent-directory creation.
- Existing config bytes are not replaced by defaults.
- The active API key never appears in error text, JSONL output, or newly written session JSONL; unsafe key values are rejected before output.
- The configured provider API-key environment variable is removed from every Lucy child environment, including context-discovery helpers and `cmd` shells.
- Early fallback diagnostics scrub every non-empty inherited environment value, including short values; missing-key diagnostics do not echo the configured environment-variable name.
- A resumed session whose current key is already present in its raw file is rejected rather than sent to the provider or exposed by listing.
- Config parse errors identify the setting/file without echoing secret values.
- A session's resolved prompt remains stable across resume.

## Alternatives and trade-offs

A compiled prompt would be simpler but violate user ownership. An installer-specific post-install step would not cover direct binary use or `cargo install`. Storing API keys in TOML would be convenient but creates a durable secret-leak surface.

## Consequences

The first run mutates the user's home directory. Users must start a new session for prompt/config changes to apply. Credential rotation does not migrate old-key session data; legacy data containing an old inactive key remains a user-managed residual. Provider-specific optional headers are out of scope for v1.

## Enforcement

Tests MUST cover first-run creation, no-overwrite behavior, config parsing, environment-key lookup, redaction, and prompt snapshot stability.

## Revisit when

Reconsider this decision if Lucy gains managed credentials, multiple profiles, project-local configuration, installer-specific distribution, or provider-specific features that cannot fit the OpenAI-compatible request shape.
