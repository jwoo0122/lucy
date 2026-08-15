# Migrate legacy ADR schema v2

Use this playbook only when `adr/.adr-system.yaml` declares exactly:

```yaml
schema: maintain-architecture-decisions
version: 2
```

Other unknown schemas are not v2 and must not be guessed into this migration.

## Authority and bootstrap boundary

A legacy directory cannot pass the current pre-change gate. Treat semantic migration as a narrowly scoped bootstrap exception:

- If the user requested ADR migration, proceed within that authority.
- If migration was discovered during another change, stop and ask whether migration may be added to scope.
- Until the migration passes the new global gate, change only `adr/`, ADR contract tests or checks required by the migrated invariants, and a repository-owned ADR command or CI wiring when needed.
- Do not resume the original change until migration passes.

Confirm that the repository can be rolled back, normally through clean Git history or an explicit backup. Preserve unrelated user changes.

## Semantic migration

Read every legacy record, including proposed, accepted, superseded, and retired records. For each accepted record, inspect its decision question, current decision, prose invariants, enforcement entries, exception, related source, tests, CI, and linked ADRs.

1. Identify the durable properties that the decision actually requires.
2. Give each property a stable meaning-based ID and statement.
3. Map each invariant exactly once to executable or manual enforcement.
4. Register meaningful executable checks in `.adr-system.yaml`.
5. Preserve or repair semantic relationships and stable record paths.
6. Replace the marker only after records and checks are ready for the new schema.
7. Reindex, validate, and run the global check.

Do not assume one legacy enforcement entry equals one invariant. One string assertion may weakly represent several properties, and several assertions may represent one property.

Choose invariant IDs for durable meaning:

```yaml
# Bad: describes the old implementation trace
- id: skill-md-contains-gate-text

# Good: describes the required property
- id: global-pre-post-gate
```

## Enforcement conversion

Use `executable` only when a deterministic check observes the claimed property. Prefer parsed structure, dependency graphs, schemas, public contract tests, negative cases, migrations in disposable environments, or other behavioral evidence.

```yaml
invariants:
  - id: sole-public-entry
    statement: "The package contains exactly one intended public entry point."
enforcement:
  - invariant: sole-public-entry
    kind: executable
    check: package-contract
```

Use `manual` when deterministic portable verification is genuinely unavailable. State the limitation, inspectable evidence, and automation trigger.

```yaml
enforcement:
  - invariant: global-pre-post-gate
    kind: manual
    reason: "Repository state cannot prove agent actions at both workflow boundaries."
    evidence:
      - tests/scenarios.json#pre-post-global-gate
    revisit_when:
      - "The harness exposes machine-verifiable lifecycle hooks."
```

Converting executable enforcement to manual can weaken evidence. Explain and question any material weakening rather than hiding it in migration.

## Prohibited shortcuts

Do not:

- update only the marker;
- wrap every old substring assertion in a test and call it semantic enforcement;
- convert all enforcement to manual;
- use `reason: legacy migration` as a manual justification;
- delete accepted records or invariants to make the gate pass;
- merge or split stable questions without semantic reason;
- treat current source drift as the accepted intent;
- treat authority for the original task as authority to redesign all ADRs.

Ask the user when source, accepted intent, or the proper invariant remains materially ambiguous.

## Completion

Prefer the repository's ADR interface; otherwise use the bundled reference client.

```sh
scripts/adr reindex --root .
scripts/adr validate --root .
scripts/adr check --root .
```

Migration is complete only when the new global gate passes, manual invariants are reported honestly, and the diff preserves the decisions' meaning. Summarize semantic changes, enforcement strengthened or weakened, unresolved manual claims, and any required CI update before resuming the original request.
