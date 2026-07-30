# Diagnostics

`lucy doctor` checks Lucy's configuration, protected storage paths, authentication prerequisites, provider metadata, shell boundary, terminal state, and public protocol without creating a conversation session.

```sh
lucy doctor
lucy doctor --json
```

Human-readable output is written to stderr. `--json` writes a versioned report to stdout. A report exits successfully when no check has `fail` status; warnings and skipped checks do not make it fail.

## Network and process effects

The ordinary command contacts provider metadata/catalog endpoints when configuration and authentication are valid. It may refresh Codex credentials as part of provider initialization or metadata access. It also runs a bounded shell probe for API-key providers to check ordinary environment inheritance and removal of the active provider variable. The probe uses Lucy's normal command timeout/output bounds and does not execute model-selected command text.

`lucy doctor --live` additionally sends one provider inference request. It can incur provider cost. The request includes Lucy's `cmd` schema, but Lucy does not execute a tool call returned by that diagnostic request. Neither mode creates a normal conversation session.

## Secret handling

Diagnostics load the active API key or Codex access token when needed. Before report serialization, Lucy applies exact-secret redaction to check messages and nested detail values. Early fallback diagnostics also scrub inherited non-empty environment values. Provider and OAuth paths avoid including response bodies in common errors.

This is output redaction, not credential isolation. It does not detect transformed, encoded, split, partial, or independently retrieved values. Do not attach configuration, credential, or session files to reports without reviewing them separately.

## What doctor does not establish

A passing report does not mean that Lucy is sandboxed, that a repository is trustworthy, or that model-selected commands are safe. In particular, `lucy doctor` does not inspect or execute the configured deny-policy hook and does not prove that its path, dependencies, or command matching are effective. Review the [command policy](command-policy.md) separately.

For the authority granted to the model, command and persistence bounds, credential limitations, and recommended external isolation, read the [trust model](trust-model.md).
