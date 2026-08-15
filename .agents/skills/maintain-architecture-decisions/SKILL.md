---
name: maintain-architecture-decisions
description: Internal ADR reference client for architect. Reconcile semantic living decisions, require honest invariant-level enforcement, and run global conformance checks. Not a user-facing entry point.
---

# Maintain architecture decisions

Act as `architect`'s internal reference client. The repository owns `adr/`; any model or tool may maintain it if it preserves the contract in `adr/README.md`.

Read `references/decision-policy.md` before editing. For legacy `maintain-architecture-decisions` version `2`, also follow `references/migrate-legacy-v2.md`; never perform a marker-only conversion.

## Reconcile decisions

- Use `index.yaml` to route to relevant records.
- Keep one record per stable design question; improve or revise it instead of appending chronology.
- Create a record only for a durable, constraining, non-obvious decision.
- Keep relationships valid and supersession bidirectional.
- Let a current explicit user decision revise accepted intent; never rewrite intent solely because source drifted.
- Keep one logical ADR writer during a change; other contexts report evidence and conflicts.
- Classify the effect as `none`, `reference`, `improve`, `revise`, `create`, `supersede`, or `retire`, then update `last_reviewed` and reindex when meaning or enforcement changes.

## Enforce invariants

Every accepted invariant has exactly one mapping:

- `executable`: a registered deterministic check, run without a shell from the repository root;
- `manual`: a concrete reason, inspectable evidence, and automation revisit conditions, reported as not mechanically verified.

Do not represent source-string or prose presence as semantic conformance. Prefer contract tests, parsed structure, schemas, dependency graphs, negative cases, simulations, or other checks that observe the claimed property. Never weaken executable enforcement, convert it to manual, or add an exception merely to pass. Return unresolved intent, scope, or source conflicts to `architect`.

## Check

Prefer the repository's ADR command (for example, `scripts/adr check --root .`); otherwise use the bundled `scripts/adr` launcher in this skill. A global check validates structure and index freshness, requires complete invariant coverage, executes each referenced check once, reports manual invariants, and fails on any error.
