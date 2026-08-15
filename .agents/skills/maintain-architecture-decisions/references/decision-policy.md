# Decision policy

`adr/` is repository-owned architecture data. This skill is one reference client, not an exclusive writer or semantic authority. Another model or tool may maintain the records and replace the checker when it preserves the self-described data and conformance contract in `adr/README.md`.

## What belongs in ADRs

Record a decision when it is durable, constrains future work, and is not obvious from implementation and tests alone. Typical examples establish authority or ownership, contracts, compatibility, security or privacy boundaries, data lifecycle, failure semantics, or expensive-to-reverse constraints.

Do not record local reversible implementation choices, routine use of established conventions, implementation logs, speculative alternatives, or facts already expressed completely by code and tests.

## Semantic maintenance

- One stable design question has one owning record.
- Improve prose without changing intent as `improve`.
- Change the current answer as `revise` and record the new rationale.
- Use `supersede` only when question boundaries change; maintain both relationship directions.
- Use `retire` when no successor decision is needed.
- Do not preserve obsolete instructions as accepted history.

## Evidence precedence

A current explicit user decision can revise accepted intent. Accepted ADR intent outranks accidental source state. Source and tests are evidence and may show that an ADR is stale, but drift alone does not authorize rewriting the decision.

## Enforcement quality

Each accepted invariant has a stable ID and exactly one enforcement mapping.

Prefer checks that parse structure, inspect dependency graphs, exercise public contracts and negative cases, validate schemas, apply migrations in disposable environments, or otherwise observe the property claimed by the invariant. A file substring usually proves only that text exists, not that behavior conforms.

Use `manual` only when deterministic portable verification is genuinely unavailable. State why, identify inspectable evidence, and name conditions that should trigger automation. Manual invariants pass structural CI but are always reported as not mechanically verified.

Registry commands live in `adr/.adr-system.yaml` as argv arrays. The exact token `{python}` resolves to the compatible interpreter running the ADR tool. They run without a shell, once per check ID, with the repository root as working directory. They must be non-interactive and must not recursively invoke `adr check`. Environment preparation and dependency installation belong to the repository's CI or build tooling, not the ADR checker.
