---
id: harness.configuration-and-provider
status: accepted
scope: harness
decision_type: configuration
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy bootstraps a user-editable XDG config file, migrates the legacy ~/.lucy/config.toml once when needed, and reads provider credentials only from the environment.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
supersedes: []
superseded_by: []
last_reviewed: "2026-07-19"
---

# User-owned configuration and provider boundary

## Decision question

Where do Lucy's minimal system prompt and LLM connection settings live, and when do they take effect?

## Current decision

Lucy MUST create `$XDG_CONFIG_HOME/lucy/config.toml` on first run when it does not exist. When `XDG_CONFIG_HOME` is unset or empty, Lucy MUST use `~/.config/lucy/config.toml`. If that XDG destination does not exist and the legacy `~/.lucy/config.toml` exists, Lucy MUST securely migrate the legacy bytes to the destination before bootstrap. Lucy MUST never overwrite an existing XDG destination or legacy file during bootstrap or upgrade. The file MUST expose a user-editable `system_prompt` plus `[llm]` settings for `base_url`, `model`, `api_key_env`, and an optional `effort`.

The generated prompt MUST be minimal and editable:

- the model can access computer resources;
- it should use the provided tools to achieve the user's requirements;
- it should read a relevant skill's `SKILL.md` with `cmd` when needed.

The generated config SHOULD use OpenRouter's OpenAI-compatible endpoint as its example/default base URL, while all compatible endpoints remain configurable. The generated model value MUST be empty so Lucy does not guess a time-sensitive provider model; starting a session without a model MUST fail with a clear configuration error. API credentials MUST be read from the configured environment variable and MUST NOT be stored in config, session files, protocol events, or diagnostics. A credential containing JSON syntax/control characters, only decimal digits, or a complete fixed protocol/storage literal MUST be rejected before it can enter serialized output; these values cannot be safely redacted while preserving the schema. Newly created session headers MUST also reject any cwd or LLM setting containing the active credential. The generated OpenRouter example uses `OPENROUTER_API_KEY`; the runtime default credential variable is `OPENAI_API_KEY` when `api_key_env` is omitted.

When `effort` is set to a non-empty value, Lucy MUST send it verbatim as the OpenAI Chat Completions `reasoning_effort` request field; when it is unset or omitted, Lucy MUST NOT send the field. Lucy MUST NOT validate `effort` against a fixed enum — compatibility is the user's responsibility, and a value the configured provider or model rejects is a runtime provider error, not a boot failure. An empty or whitespace-only `effort` MUST fail boot with a configuration error. The resolved `effort` is sent with each request when set.

`config.toml` is the source of truth for model and effort whenever a session starts or resumes. The interactive TUI MUST provide an idle-only `/settings` menu that reads the configured provider catalog, supports typed model filtering plus keyboard selection, and writes selected model/effort values back to config before applying them to the current session. Catalog capability metadata MAY provide a finite effort picker; when it does not, the UI MUST accept a user-entered effort value. A resumed session MUST reload the current config model and effort rather than reuse the header values. The session header and every interactive setting transition MUST retain a secret-safe timestamped provider-settings audit record so historical requests remain attributable without making the header authoritative.

Lucy MUST resolve config and ambient context at new-session boot and persist the resolved system prompt in the session snapshot. Editing config does not change the resolved prompt, cwd, or skill snapshot of an existing session.

## Context and forces

Users need to inspect and change the minimal model guidance without recompiling Lucy. Cargo installation has no portable user-home post-install hook, so first-run bootstrap is the reliable installation-independent behavior. The XDG base directory convention separates configuration from Lucy's legacy session storage while retaining a predictable user-editable location. Credentials are secrets and should not enter durable user-controlled artifacts or the direct command environment. Command execution remains useful through the rest of the inherited process environment, without granting the shell the provider credential directly. This is not OS-level process isolation: parent-process inspection and transformed side channels remain outside the v1 guarantee.

## Invariants

- Missing XDG config is created once with safe parent-directory creation.
- An unset, empty, or relative `XDG_CONFIG_HOME` resolves to `~/.config`; a non-empty absolute XDG home determines the configuration root.
- When no XDG config exists, a regular non-symlink legacy `~/.lucy/config.toml` is migrated once without changing its bytes; an existing XDG config always wins.
- Existing config bytes are not replaced by defaults.
- The active API key never appears in error text, JSONL output, or newly written session JSONL; unsafe key values are rejected before output.
- The configured provider API-key environment variable is removed from every Lucy child environment, including context-discovery helpers and `cmd` shells.
- Early fallback diagnostics scrub every non-empty inherited environment value, including short values; missing-key diagnostics do not echo the configured environment-variable name.
- A resumed session whose current key is already present in its raw file is rejected rather than sent to the provider or exposed by listing.
- The session header and every provider-settings audit record are secret-safe; an effort containing the active provider key is rejected like other provider-setting values.
- Model and effort are reloaded from `config.toml` on every new or resumed session; the session audit trail records rather than overrides those selections.
- `/settings` is available only when the TUI has no active turn, and provider catalog failures must not expose credentials.
- Config parse errors identify the setting/file without echoing secret values.
- A session's resolved prompt remains stable across resume.

## Alternatives and trade-offs

A compiled prompt would be simpler but violate user ownership. An installer-specific post-install step would not cover direct binary use or `cargo install`. Storing API keys in TOML would be convenient but creates a durable secret-leak surface.

## Consequences

The first run mutates the user's XDG configuration directory (or `~/.config` by default). Upgrading an installation with only a legacy config moves that config to the XDG location; sessions remain in `~/.lucy/sessions`. Model and effort changes made through `/settings` affect the next request in the current idle session and become the defaults for new or resumed sessions; prompt changes still require a new session. Credential rotation does not migrate old-key session data; legacy data containing an old inactive key remains a user-managed residual. Provider-specific optional headers are out of scope for v1.

## Enforcement

Tests MUST cover XDG and default-path first-run creation, legacy-config migration, no-overwrite behavior, config parsing, environment-key lookup, redaction, prompt snapshot stability, provider-catalog fallback behavior, settings persistence, resume-time model/effort reload, and provider-settings audit records.

## Revisit when

Reconsider this decision if Lucy gains managed credentials, multiple profiles, project-local configuration, installer-specific distribution, or provider-specific features that cannot fit the OpenAI-compatible request shape.
