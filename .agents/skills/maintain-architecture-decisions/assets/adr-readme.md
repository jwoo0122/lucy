# Architecture decisions

This directory is repository-owned architecture data. It is not owned by a particular model, skill, or checker. Any model or tool may maintain it if the resulting repository preserves this contract. The tool that initialized the directory is only a reference client.

## Data model

- `.adr-system.yaml` identifies the `semantic-living-adr` protocol version and registers executable checks as argv arrays.
- `index.yaml` is a deterministic generated routing map; records are authoritative when it is stale.
- `records/<scope>/<stable-question>.md` owns one stable design question.
- `_template.md` defines record frontmatter and required prose sections.
- Records are revised as current intent changes rather than appended as chronology.
- Decision relationships refer to semantic record IDs, and supersession is bidirectional.
- Record IDs use `<scope>.<stable-question>` with lowercase letters, digits, dots, and hyphens; the file path is `records/<id with dots as slashes>.md`.
- Status is `proposed`, `accepted`, `superseded`, or `retired`. Only accepted records impose current constraints.
- Required frontmatter fields are listed in `_template.md`.

## Accepted-record contract

Every accepted record declares stable invariants and maps each exactly once to:

- `executable`: a deterministic registered check; or
- `manual`: a reason, inspectable evidence, and a condition for reconsidering automation.

Manual invariants are not mechanically verified. Source-string presence alone is not proof of architectural conformance.

Use `index.yaml` to select relevant records. Prefer revising the record that owns an existing question. Keep one logical writer during a change to avoid conflicting edits; this is a concurrency rule, not tool ownership.

A conforming implementation validates structure and relationships, generates the index deterministically, checks complete invariant coverage, executes each referenced check once from the repository root without a shell, and reports manual invariants honestly. Repositories should expose their chosen implementation through a stable repository-level command for agents and CI.
