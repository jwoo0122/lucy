# Lucy JSONL Protocol v1

Lucy exposes a versioned JSONL protocol for external programs. The TUI and JSONL surfaces share the same turn engine and normalized event model.

## Transport

- **stdout**: protocol JSONL only. One line is one complete JSON object.
- **stderr**: diagnostics and human-readable errors. Never parsed by protocol consumers.
- **stdin**: JSONL message records from the caller.
- **Encoding**: UTF-8.
- **Blank lines** on stdin are ignored.

## Handshake

The first stdout record is always a `protocol` event:

```json
{"type":"protocol","version":1,"capabilities":["sessions","cancellation","background_commands"]}
```

Consumers must verify the version before processing further events. Unknown capabilities must be ignored.

## Input records

```json
{"type":"message","text":"Run the tests.","request_id":"build-42"}
```

| Field | Required | Description |
|-------|----------|-------------|
| `type` | yes | Must be `"message"`. |
| `text` | yes | The user message text. May be empty. |
| `request_id` | no | Caller-owned correlation identifier. Propagated to output events. |

- Malformed JSON: a recoverable error is emitted on stdout as an `error` event; the process continues.
- Unknown `type` values: rejected with an `error` event.
- Unknown fields: ignored (forward-compatible).

## Output events

All events are JSON objects with a `type` discriminator.

### `session`
```json
{"type":"session","session_id":"abc123","resumed":false}
```

### `assistant_delta`
```json
{"type":"assistant_delta","text":"Hello","turn_id":"turn-1","request_id":"build-42"}
```

`turn_id` and `request_id` are optional. They are omitted when no correlation is available.

### `tool_call`
```json
{"type":"tool_call","id":"call-1","name":"cmd","arguments":"{\"command\":\"ls\"}","turn_id":"turn-1","request_id":"build-42"}
```

### `tool_result`
```json
{"type":"tool_result","id":"call-1","name":"cmd","result":{"exit_code":0,"stdout":"...","stderr":""},"turn_id":"turn-1","request_id":"build-42"}
```

### `turn_end`
```json
{"type":"turn_end","turn_id":"turn-1","request_id":"build-42"}
```

The model will not produce more output until a new message is received.

### `turn_interrupted`
```json
{"type":"turn_interrupted","reason":"user_cancelled","phase":"provider_stream","turn_id":"turn-1","request_id":"build-42"}
```

### `error`
```json
{"type":"error","message":"unable to start session","request_id":"build-42"}
```

Recoverable: the process continues. Terminal: the process exits with non-zero status.

## Event ordering

A normal turn emits:
1. Zero or more `assistant_delta` events (streamed text)
2. Zero or more `tool_call` + `tool_result` pairs (model may make multiple tool rounds)
3. Exactly one `turn_end`

An interrupted turn emits:
1. Events produced before the interruption point
2. Exactly one `turn_interrupted`

Background command follow-up turns start automatically after a background command completes. They produce the same event sequence as a normal turn. The `request_id` from the originating turn is not propagated to automatic follow-up turns.

## Correlation

- `request_id`: caller-owned, optional. Propagated from the input message to all events in the resulting turn.
- `turn_id`: Lucy-assigned, session-local. Unique per turn, including automatic follow-up turns.
- Tool call `id`: provider-assigned tool call identifier.

## Process exit codes

- `0`: clean exit after stdin EOF and all background commands complete.
- Non-zero: fatal error or session failure.

## stdin EOF

When stdin closes while background commands are still running, Lucy waits for them to complete, emits their results, and then exits. If stdin closes with no active background commands, Lucy exits immediately.

## Cancellation

Cancellation is available when the `cancellation` capability is advertised. The mechanism depends on the frontend:
- TUI: Esc or Ctrl-C.
- JSONL: process signal (SIGINT/SIGTERM).

Cancellation during provider streaming or command execution produces a `turn_interrupted` event.

## Session semantics

- New session: Lucy creates a session and emits `session` with `resumed: false`.
- Resumed session: Lucy loads the session and emits `session` with `resumed: true`.
- Concurrent access to one persisted session is not coordinated. Multiple processes may read the same session, but concurrent writes may conflict.

## Compatibility policy

For protocol v1:
- Required fields must not be removed or redefined.
- New optional fields may be added.
- Consumers must ignore unknown optional fields.
- Unknown event types must be ignored.
- Incompatible semantic changes require a new protocol version.

## Excluded data

The following provider-specific data is intentionally never emitted:
- Raw provider response objects
- Private reasoning metadata
- Provider API keys or credentials
- Internal session file format details
