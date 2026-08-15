---
id: <scope>.<stable-question>
status: proposed
scope: <scope>
decision_type: <type>
applies_to:
  - path/**
summary: "One-sentence current decision"
constrains: []
depends_on: []
supersedes: []
superseded_by: []
last_reviewed: "YYYY-MM-DD"
invariants:
  - id: stable-invariant-id
    statement: "A durable property this decision requires."
enforcement:
  - invariant: stable-invariant-id
    kind: executable
    check: registered-check-id
# Or, when deterministic verification is genuinely unavailable:
# - invariant: contextual-invariant
#   kind: manual
#   reason: "Why this cannot be checked portably and deterministically."
#   evidence:
#     - path/to/inspectable-evidence
#   revisit_when:
#     - "A condition that should trigger automation."
---

# Decision title

## Decision question

What stable architectural question does this record own?

## Current decision

State the current answer and normative boundaries.

## Context and forces

Explain why the decision exists and which trade-offs matter.

## Invariants

- `stable-invariant-id`: Repeat each frontmatter invariant with enough context for readers.

## Alternatives and trade-offs

Record the viable alternatives and why the current answer wins.

## Consequences

State operational and evolutionary effects.

## Enforcement

Explain what the registered checks observe and which claims remain manual.

## Revisit when

List concrete triggers for review.
