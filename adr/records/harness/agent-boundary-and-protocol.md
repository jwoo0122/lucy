---
id: harness.agent-boundary-and-protocol
status: accepted
scope: harness
decision_type: boundary
applies_to:
  - "Cargo.toml"
  - "src/**"
  - "tests/**"
  - "README.md"
summary: Lucy is a local macOS/Linux JSONL CLI harness with one OpenAI-compatible Chat Completions provider and one model-facing cmd tool.
constrains: []
depends_on: []
supersedes: []
superseded_by: []
last_reviewed: "2026-07-16"
---

# Local JSONL harness boundary

## Decision question

What public boundary and capability surface does the first Lucy harness expose?

## Current decision

Lucy MUST run as a local macOS/Linux process communicating through newline-delimited JSON on stdin and stdout. It MUST NOT be a network service in v1. The harness MUST expose only the `cmd` model tool; it MUST NOT provide built-in `read`, `write`, `edit`, or other file-operation tools. The LLM integration MUST target the OpenAI-compatible Chat Completions API, while keeping the base URL configurable.

Provider SSE and tool-call chunks MUST be converted into Lucy-owned normalized JSONL events. Provider-specific response chunks MUST NOT become the public protocol. The process MUST emit only JSONL events on stdout and send diagnostics to stderr. One process handles one active turn at a time.

## Context and forces

The goal is a thin, embeddable harness rather than a full coding-agent product. A local trusted model needs command execution and conversation state, but callers should not depend on OpenAI/OpenRouter chunk shapes. A local process boundary keeps integration simple without adding authentication or multi-tenant concerns.

## Invariants

- Input messages and output events are LF-delimited JSON records.
- A successful turn exposes assistant deltas, normalized tool calls/results, and an explicit turn completion event.
- A model tool call is executed by the harness before the next provider turn.
- The active provider key is not emitted in protocol events or diagnostics; key values that cannot be safely represented are rejected before output.
- No network listener, authentication layer, approval UI, or sandbox is required by this boundary.

## Alternatives and trade-offs

A library, HTTP server, or raw provider-stream pass-through would increase coupling or implementation surface. Additional file tools would make Lucy a larger coding agent and are intentionally left to future extensions or callers.

## Consequences

Clients must implement a small event consumer, but they remain independent of provider wire formats. The single active turn rule avoids multiplexing and ordering semantics in the first protocol.

## Enforcement

Integration tests MUST exercise JSONL input/output, normalized text streaming, the cmd tool loop, and stdout purity. Tests MUST verify that a provider-specific stream is not forwarded as a public event.

## Revisit when

Reconsider this decision if callers require concurrent sessions in one process, a remote deployment, multiple providers with incompatible tool protocols, or first-class file operations.
