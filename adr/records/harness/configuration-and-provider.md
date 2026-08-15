---
id: harness.configuration-and-provider
status: accepted
scope: harness
decision_type: configuration
applies_to:
- src/**
- tests/**
- README.md
summary: Lucy owns a compiled built-in boot prompt, bootstraps provider settings in an XDG config file, preserves valid legacy system_prompt keys while ignoring them, and reads provider credentials only from the environment.
constrains: []
depends_on:
- harness.agent-boundary-and-protocol
supersedes: []
superseded_by: []
last_reviewed: '2026-08-04'
enforcement:
- invariant: create-missing-config-once
  kind: executable
  check: rust-tests
- invariant: generated-config-excludes-prompt
  kind: executable
  check: rust-tests
- invariant: xdg-config-root-resolution
  kind: executable
  check: rust-tests
- invariant: non-destructive-legacy-config-migration
  kind: executable
  check: rust-tests
- invariant: existing-config-not-replaced
  kind: executable
  check: rust-tests
- invariant: legacy-prompt-is-inert
  kind: executable
  check: rust-tests
- invariant: serialized-api-key-confidentiality
  kind: executable
  check: rust-tests
- invariant: child-credential-boundary
  kind: executable
  check: rust-tests
- invariant: early-diagnostic-scrubbing
  kind: executable
  check: rust-tests
- invariant: unsafe-resume-rejected
  kind: executable
  check: rust-tests
- invariant: secret-safe-settings-audit
  kind: executable
  check: rust-tests
- invariant: config-authoritative-on-start
  kind: executable
  check: rust-tests
- invariant: idle-settings-and-safe-catalog-errors
  kind: executable
  check: rust-tests
- invariant: safe-config-errors
  kind: executable
  check: rust-tests
- invariant: immutable-boot-prompt-snapshot
  kind: executable
  check: rust-tests
- invariant: stable-provider-session-identity
  kind: executable
  check: rust-tests
- invariant: provider-specific-metadata-boundary
  kind: executable
  check: rust-tests
invariants:
- id: create-missing-config-once
  statement: Lucy creates a missing XDG config once with safe parent-directory creation.
- id: generated-config-excludes-prompt
  statement: Generated config contains provider settings and omits system_prompt.
- id: xdg-config-root-resolution
  statement: Unset, empty, or relative XDG_CONFIG_HOME resolves to ~/.config, while a non-empty absolute value determines the config root.
- id: non-destructive-legacy-config-migration
  statement: When no XDG config exists, a regular non-symlink legacy config is migrated once byte-for-byte, and an existing XDG config always wins.
- id: existing-config-not-replaced
  statement: Existing config bytes are not replaced by defaults.
- id: legacy-prompt-is-inert
  statement: A valid legacy system_prompt is accepted, ignored, and preserved through unrelated settings writes.
- id: serialized-api-key-confidentiality
  statement: The active API key is absent from errors, JSONL output, and newly written sessions, and unsafe key values are rejected before output.
- id: child-credential-boundary
  statement: Context discovery retains the configured OpenRouter key environment, cmd children remove it, serialized output redacts it, and Codex tokens are never placed in child environments.
- id: early-diagnostic-scrubbing
  statement: Early fallback diagnostics scrub inherited environment values and missing-key diagnostics do not echo the configured variable name.
- id: unsafe-resume-rejected
  statement: A resumed session containing the current key is rejected rather than sent to a provider or exposed by listing.
- id: secret-safe-settings-audit
  statement: Session headers and provider-settings audit records reject provider settings containing the active key.
- id: config-authoritative-on-start
  statement: Model and effort are reloaded from config for every new or resumed session, while the session audit trail records those selections.
- id: idle-settings-and-safe-catalog-errors
  statement: The settings UI is available only while the TUI is idle and provider-catalog failures do not expose credentials.
- id: safe-config-errors
  statement: Config parse errors identify the setting or file without echoing secret values.
- id: immutable-boot-prompt-snapshot
  statement: New sessions snapshot the current built-in composed prompt and resumed sessions retain their historical boot_system_prompt.
- id: stable-provider-session-identity
  statement: Provider routing or cache identity equals the Lucy session ID and remains stable across resume, compaction, and settings changes.
- id: provider-specific-metadata-boundary
  statement: OpenRouter and Codex receive only their documented identity metadata, while generic compatible endpoints receive no provider-specific identity fields or headers.
---

# Lucy-owned prompt and user-owned provider configuration

## Decision question

Where do Lucy's built-in boot system prompt and LLM connection settings live, and when do they take effect?

## Current decision

Lucy MUST own a compiled built-in boot system prompt. `config.toml` MUST NOT expose or control `system_prompt`. Deprecated Rust API compatibility members MAY retain the former name, but MUST be excluded from Serde and MUST NOT affect boot prompt composition. A syntactically valid existing config that contains a legacy top-level `system_prompt` key MUST remain valid; Lucy MUST ignore that value. Reading, migration, and bootstrap MUST NOT rewrite an existing valid config. An unrelated settings write MUST preserve the legacy entry and its string value rather than remove it or make it effective. A newly generated config MUST omit the key.

Lucy MUST create `$XDG_CONFIG_HOME/lucy/config.toml` on first run when it does not exist. When `XDG_CONFIG_HOME` is unset or empty, Lucy MUST use `~/.config/lucy/config.toml`. If that XDG destination does not exist and the legacy `~/.lucy/config.toml` exists, Lucy MUST securely migrate the legacy bytes to the destination before bootstrap. Lucy MUST never overwrite an existing XDG destination or legacy file during bootstrap or upgrade. The file MUST expose an `[auth]` selection for the OpenRouter or Codex subscription provider and `[llm]` settings for `base_url`, `model`, and an optional `effort`. OpenRouter retains an environment-variable API-key setting; Codex subscription credentials MUST not be placed in config.

The generated config SHOULD use OpenRouter's OpenAI-compatible endpoint as its example/default base URL, while all compatible endpoints remain configurable. The generated model value MUST be empty so Lucy does not guess a time-sensitive provider model; starting a session without a model MUST fail with a clear configuration error. API credentials MUST be read from the configured environment variable and MUST NOT be stored in config, session files, protocol events, or diagnostics. A credential containing JSON syntax/control characters, only decimal digits, or a complete fixed protocol/storage literal MUST be rejected before it can enter serialized output; these values cannot be safely redacted while preserving the schema. Newly created session headers MUST also reject any cwd or LLM setting containing the active credential. The generated OpenRouter example uses `OPENROUTER_API_KEY`; the runtime default credential variable is `OPENAI_API_KEY` when `api_key_env` is omitted. Codex subscription authentication uses Lucy’s private credential store and the explicit `lucy codex login`/`logout` commands.

When `effort` is set to a non-empty value, Lucy MUST send it verbatim as the OpenAI Chat Completions `reasoning_effort` request field; when it is unset or omitted, Lucy MUST NOT send the field. Lucy MUST NOT validate `effort` against a fixed enum — compatibility is the user's responsibility, and a value the configured provider or model rejects is a runtime provider error, not a boot failure. An empty or whitespace-only `effort` MUST fail boot with a configuration error. The resolved `effort` is sent with each request when set.

Every model request in one named Lucy session MUST use that session's existing public session ID as a stable, secret-safe provider routing/cache identity when the selected provider supports one. Resume, tool follow-up, compaction, and post-settings-change requests MUST retain the same identity. Requests to OpenRouter's own host MUST send it as the top-level `session_id` and identify the application with `X-OpenRouter-Title: Lucy` and `HTTP-Referer: https://lucyna.run`. Codex subscription Responses requests MUST send it as `prompt_cache_key` and identify the application with `originator: lucy`. Generic OpenAI-compatible endpoints MUST NOT receive these provider-specific fields or headers. Lucy does not derive parent/task identities or manage routing identities for external agents, including independently launched Lucy processes.

`config.toml` is the source of truth for model and effort whenever a session starts or resumes. The interactive TUI MUST provide an idle-only `/settings` menu that reads the configured provider catalog, supports typed model filtering plus keyboard selection, and writes selected model/effort values back to config before applying them to the current session. Catalog capability metadata MAY provide a finite effort picker; when it does not, the UI MUST accept a user-entered effort value. A resumed session MUST reload the current config model and effort rather than reuse the header values. The session header and every interactive setting transition MUST retain a secret-safe timestamped provider-settings audit record so historical requests remain attributable without making the header authoritative.

Lucy MUST compose its current compiled built-in prompt with ambient context at new-session boot and persist the result in the session snapshot. Editing config cannot change the prompt of a new or existing session. A resumed session MUST retain its historical `boot_system_prompt` even when the compiled built-in prompt has changed since that session was created.

## Context and forces

Boot guidance is part of Lucy's product behavior and must evolve with the binary rather than vary through user configuration. Compatibility still requires accepting existing valid configuration and preserving its bytes; silently rewriting or rejecting a legacy `system_prompt` would turn the ownership change into destructive migration. Cargo installation has no portable user-home post-install hook, so first-run bootstrap remains the reliable installation-independent behavior for provider settings. The XDG base directory convention separates configuration from Lucy's legacy session storage while retaining a predictable user-editable location. Credentials are secrets and should not enter durable user-controlled artifacts or serialized command output. Codex subscription refresh credentials require a private store outside config. Context-discovery children inherit the terminal environment, while `cmd` children inherit ordinary variables but have the configured provider credential removed before spawn; captured output is redacted before persistence. This is not OS-level process isolation: parent-process inspection and transformed side channels remain outside the v1 guarantee.

## Invariants

- Lucy creates a missing XDG config once with safe parent-directory creation.
- Generated config contains provider settings and omits system_prompt.
- Unset, empty, or relative XDG_CONFIG_HOME resolves to ~/.config, while a non-empty absolute value determines the config root.
- When no XDG config exists, a regular non-symlink legacy config is migrated once byte-for-byte, and an existing XDG config always wins.
- Existing config bytes are not replaced by defaults.
- A valid legacy system_prompt is accepted, ignored, and preserved through unrelated settings writes.
- The active API key is absent from errors, JSONL output, and newly written sessions, and unsafe key values are rejected before output.
- Context discovery retains the configured OpenRouter key environment, cmd children remove it, serialized output redacts it, and Codex tokens are never placed in child environments.
- Early fallback diagnostics scrub inherited environment values and missing-key diagnostics do not echo the configured variable name.
- A resumed session containing the current key is rejected rather than sent to a provider or exposed by listing.
- Session headers and provider-settings audit records reject provider settings containing the active key.
- Model and effort are reloaded from config for every new or resumed session, while the session audit trail records those selections.
- The settings UI is available only while the TUI is idle and provider-catalog failures do not expose credentials.
- Config parse errors identify the setting or file without echoing secret values.
- New sessions snapshot the current built-in composed prompt and resumed sessions retain their historical boot_system_prompt.
- Provider routing or cache identity equals the Lucy session ID and remains stable across resume, compaction, and settings changes.
- OpenRouter and Codex receive only their documented identity metadata, while generic compatible endpoints receive no provider-specific identity fields or headers.

## Alternatives and trade-offs

A user-editable prompt would permit local customization, but would make Lucy's boot behavior depend on mutable configuration and prevent a binary release from owning its complete default agent contract. Rejecting or deleting the legacy key would make the transition simpler internally but would break valid existing configs or rewrite user bytes. An installer-specific post-install step would not cover direct binary use or `cargo install`. Storing API keys in TOML would be convenient but creates a durable secret-leak surface.

## Consequences

The first run mutates the user's XDG configuration directory (or `~/.config` by default). Upgrading an installation with only a legacy config moves that config to the XDG location; sessions remain in `~/.lucy/sessions`. Existing `system_prompt` text may remain visible in a legacy config but has no effect; newly generated configs do not suggest that it is configurable. Model and effort changes made through `/settings` affect the next request in the current idle session and become the defaults for new or resumed sessions. A built-in prompt change affects newly created sessions only. Credential rotation does not migrate old-key session data; legacy data containing an old inactive key remains a user-managed residual. Provider-specific optional headers remain out of scope except for the fixed application-attribution and session-routing metadata above.

## Enforcement

Tests MUST cover XDG and default-path first-run creation, generated-config omission of `system_prompt`, non-destructive acceptance and runtime ignoring of the valid legacy key, legacy-config migration, no-overwrite behavior, config parsing, environment-key lookup, redaction, built-in prompt snapshot stability, provider-catalog fallback behavior, settings persistence, resume-time model/effort reload, and provider-settings audit records. Provider request tests MUST verify stable session identity for ordinary, resumed, compaction, and settings-change paths; provider-specific application attribution; and omission from generic compatible requests and public/persisted formats.

## Revisit when

Reconsider this decision if Lucy intentionally adds a supported prompt-extension mechanism, managed credentials, multiple profiles, project-local configuration, installer-specific distribution, or provider-specific features that cannot fit the OpenAI-compatible request shape.
