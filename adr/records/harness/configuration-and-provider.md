---
id: harness.configuration-and-provider
status: accepted
scope: harness
decision_type: configuration
applies_to:
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy owns a compiled built-in boot prompt, bootstraps provider settings in an XDG config file, preserves valid legacy system_prompt keys while ignoring them, and reads provider credentials only from the environment.
constrains: []
depends_on:
  - harness.agent-boundary-and-protocol
supersedes: []
superseded_by: []
last_reviewed: "2026-08-04"
enforcement:
  - id: config-bootstrap-and-migration
    path: src/config.rs
    must_contain:
      - "fn bootstraps_config_without_overwriting_existing_bytes()"
      - "fn migrates_a_legacy_config_once_without_overwriting_xdg_config()"
      - "fn generated_config_omits_system_prompt()"
      - "fn legacy_system_prompt_is_ignored_without_rewriting_config_bytes()"
      - "fn settings_updates_preserve_legacy_system_prompt()"
      - "#[serde(skip)]"
      - "fn auth_provider_rejects_mixed_credentials()"
    must_not_contain: []
  - id: openrouter-session-identity-and-attribution
    path: src/provider.rs
    must_contain:
      - "fn openrouter_requests_include_session_and_app_metadata()"
      - "fn compatible_requests_omit_provider_specific_session_metadata()"
      - 'const APP_URL: &str = "https://lucyna.run";'
    must_not_contain: []
  - id: codex-session-identity
    path: src/codex_provider.rs
    must_contain:
      - "fn codex_request_uses_responses_shape()"
      - 'request["prompt_cache_key"] = json!(session_id);'
    must_not_contain: []
enforcement_exception: null
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

- Missing XDG config is created once with safe parent-directory creation.
- Generated config contains provider settings and does not contain `system_prompt`.
- An unset, empty, or relative `XDG_CONFIG_HOME` resolves to `~/.config`; a non-empty absolute XDG home determines the configuration root.
- When no XDG config exists, a regular non-symlink legacy `~/.lucy/config.toml` is migrated once without changing its bytes; an existing XDG config always wins.
- Existing config bytes are not replaced by defaults.
- A valid legacy top-level `system_prompt` is accepted and ignored; its entry is preserved through compatibility handling and unrelated settings writes, and it never overrides the compiled built-in prompt.
- The active API key never appears in error text, JSONL output, or newly written session JSONL; unsafe key values are rejected before output.
- The configured OpenRouter API-key environment variable remains available to context-discovery helpers but is removed from `cmd` child environments before spawn; serialized output and persisted records still redact it. Codex subscription tokens are never placed in child environments.
- Early fallback diagnostics scrub every non-empty inherited environment value, including short values; missing-key diagnostics do not echo the configured environment-variable name.
- A resumed session whose current key is already present in its raw file is rejected rather than sent to the provider or exposed by listing.
- The session header and every provider-settings audit record are secret-safe; an effort containing the active provider key is rejected like other provider-setting values.
- Model and effort are reloaded from `config.toml` on every new or resumed session; the session audit trail records rather than overrides those selections.
- `/settings` is available only when the TUI has no active turn, and provider catalog failures must not expose credentials.
- Config parse errors identify the setting/file without echoing secret values.
- A new session snapshots the current built-in composed prompt; a resumed session retains its historical `boot_system_prompt`.
- Provider routing/cache identity equals the existing Lucy session ID, remains stable across resume and compaction, and is not persisted in a new field.
- OpenRouter and Codex receive only their documented identity metadata; generic compatible endpoints remain free of provider-specific request fields and headers.

## Alternatives and trade-offs

A user-editable prompt would permit local customization, but would make Lucy's boot behavior depend on mutable configuration and prevent a binary release from owning its complete default agent contract. Rejecting or deleting the legacy key would make the transition simpler internally but would break valid existing configs or rewrite user bytes. An installer-specific post-install step would not cover direct binary use or `cargo install`. Storing API keys in TOML would be convenient but creates a durable secret-leak surface.

## Consequences

The first run mutates the user's XDG configuration directory (or `~/.config` by default). Upgrading an installation with only a legacy config moves that config to the XDG location; sessions remain in `~/.lucy/sessions`. Existing `system_prompt` text may remain visible in a legacy config but has no effect; newly generated configs do not suggest that it is configurable. Model and effort changes made through `/settings` affect the next request in the current idle session and become the defaults for new or resumed sessions. A built-in prompt change affects newly created sessions only. Credential rotation does not migrate old-key session data; legacy data containing an old inactive key remains a user-managed residual. Provider-specific optional headers remain out of scope except for the fixed application-attribution and session-routing metadata above.

## Enforcement

Tests MUST cover XDG and default-path first-run creation, generated-config omission of `system_prompt`, non-destructive acceptance and runtime ignoring of the valid legacy key, legacy-config migration, no-overwrite behavior, config parsing, environment-key lookup, redaction, built-in prompt snapshot stability, provider-catalog fallback behavior, settings persistence, resume-time model/effort reload, and provider-settings audit records. Provider request tests MUST verify stable session identity for ordinary, resumed, compaction, and settings-change paths; provider-specific application attribution; and omission from generic compatible requests and public/persisted formats.

## Revisit when

Reconsider this decision if Lucy intentionally adds a supported prompt-extension mechanism, managed credentials, multiple profiles, project-local configuration, installer-specific distribution, or provider-specific features that cannot fit the OpenAI-compatible request shape.
