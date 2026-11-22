# Tether trace format, schema v0

Status: v0, frozen 2026-11-21. Release v0.3.0 was the first to write it. See
Compatibility at the end for what the freeze promises.

## Files

```
<run>.jsonl                          one Event per line, append-only, UTF-8
<run>.blobs/blake3/<ab>/<full hex>   content-addressed payloads over 32 KB
```

`tether pack <run>` bundles both into one tar, `<run>.tar`:

```
<run>.jsonl
<run>.blobs/blake3/<ab>/<full hex>   only the blobs the trace refers to
```

A plain ustar/GNU tar, uncompressed, regular files only, trace first and
blobs in hash order, with zeroed times and owners so that packing is
deterministic. A pack holds exactly one trace. `inspect`, `verify`, `diff`
and `replay` accept either form; `resume` needs the unpacked form.

## Envelope

Every line is one JSON object with these fields, in this order:

| Field | Type | Meaning |
| --- | --- | --- |
| `v` | integer | Schema version. `0`. |
| `run` | string | Run id. |
| `seq` | integer | Dense, starts at 0, increments by exactly 1. |
| `ts` | RFC 3339 string | Wall clock at write time, UTC. |
| `step` | integer | Agent turn this event belongs to. |
| `agent` | string | Emitting agent. `"main"` for single-agent runs. |
| `parent` | integer, optional | `seq` of the request this event answers. |
| `hash` | string | `blake3:<hex>`. See Hash chain. |
| `type` | string | Event type. See below. |
| `data` | object | Type-specific payload. |

Example:

```json
{"v":0,"run":"run_01J...","seq":42,"ts":"2026-11-17T19:47:03.118Z","step":7,"agent":"main","parent":41,"hash":"blake3:...","type":"tool.call","data":{}}
```

## Hash chain

`hash` is BLAKE3 over the canonical JSON encoding of the event with its own
`hash` field replaced by the previous event's `hash` (empty string for
`seq = 0`). Canonical means: keys in the order above, no whitespace, UTF-8. The trailing
newline is not hashed.

A verifier hashes the bytes of the line as written, swapping only the `hash`
value, and does not re-serialize the event. A line that is not in canonical
form therefore fails verification. `tether verify` recomputes the chain, then
checks that every referenced blob exists and still matches its hash.

## Blobs

A payload larger than 32 KB (`INLINE_LIMIT`) is written to the blob store and
the event carries `{"blob":"blake3:<hex>","bytes":N}` in its place. `bytes` is
the size of the blob. A blob is opaque bytes; a JSON payload is stored as its
compact JSON encoding.

An inline payload that itself has the shape `{"blob": string, "bytes": integer}`
would be indistinguishable from a reference, so a writer spills it to a blob
whatever its size.

Model message history is content-addressed **per message**: `model.request.data.messages`
is a list of blob hashes, one per message. A step adds one new blob; the
history is never copied.

## Event types

| `type` | `data` |
| --- | --- |
| `run.created` | agent id, model config, `policy` (the full text of the policy TOML as one string, byte for byte; null when the policy did not come from a file), `sandbox` (`{"backend": "process"}` or the like; null when the run has no sandbox), merged tool list (each entry with its `server` and `external`), `mcp_servers` (from 0.5: one entry per server behind the tool list, `{id, transport, protocol_version, server_name, server_version}`, where `transport` is `builtin`, `stdio` or `http`; absent in traces written before 0.5 and in their replays), schema version |
| `run.state` | `{state, reason}`; state is one of `created`, `running`, `suspended`, `halted`, `finished` |
| `run.child` | `{child}` run id of a spawned sub-agent process |
| `run.finished` | outcome, totals, final budget |
| `model.request` | provider, model, `messages` (blob hashes), `tools`, params |
| `model.response` | `message` (blob hash), `tool_calls`, usage, `stop_reason` (the provider's word for why it stopped, such as `end_turn`, `tool_use`, `max_tokens`, `refusal`; absent when the adapter gives none), `latency_ms`, `ttft_ms`, `stream`, `provider_request_id`, `raw` (blob hash; the whole event stream when `stream` is true) |
| `tool.call` | id, name, server, arguments, `external` |
| `tool.result` | id, content (inline or blob), `is_error`, `duration_ms` |
| `policy.decision` | `subject` seq, `rule` (name of the rule that matched, null when none did), `decision` `{outcome: allow \| deny \| ask, ...}`, budget snapshot |
| `policy.ask` | `subject`, prompt |
| `policy.answer` | `subject`, `answered_by`, `allow` |
| `sandbox.exec` | command, cwd, env hash |
| `sandbox.exit` | exit code, stdout and stderr refs |
| `budget.tick` | tokens, usd, wall seconds, tool calls: used and limit (null limit means unlimited); after a priced model call also `price` `{usd, table}`, the cost of that call and the version of the pricing table it came from |
| `checkpoint` | `step` (the step just completed), `snapshot` (the sandbox backend's snapshot id, null when it cannot snapshot) |
| `nondeterminism` | clock reads, random seeds, env lookups |
| `ext.*` | anything; ignored by replay and diff |

## Ordering guarantees

For every tool call the trace contains, in this order and with the same
`step`: `tool.call`, `policy.decision`, then `tool.result`. A `deny` still
produces a `tool.result` with `is_error: true` and the reason.

A call to a tool that was not listed in `run.created` is recorded the same
way, with an empty `server` and a `deny` decision.

For every model call: `model.request`, `policy.decision`, `model.response`.
A denied model call, or one the provider fails, has no `model.response`; the
next event is the `run.state` that halts the run.

When the decision is `ask`, four events sit between the `policy.decision` and
the result: `policy.ask`, a `run.state` of `suspended`, `policy.answer`, and a
`run.state` of `running`. An answer of no gives a `tool.result` with
`is_error: true`; for a model call it halts the run instead. `answered_by` is
`"cli"` for the terminal prompt and `"timeout"` when a rule's
`on_timeout = "deny"` turned an unanswered ask into a denial. An ask that
goes unanswered without that setting has no `policy.answer`: the `suspended`
state is followed by `halted` with the reason `ask unanswered after <n> s`.

Every `model.response` and every `tool.result` is followed by a `budget.tick`
with the same `parent`, holding the budget after that call.

Budgets are checked before a call's first event. A run that is out of budget
has no `model.request` or `tool.call` for the refused call: the next event is
a `run.state` of `halted` whose reason is `budget exceeded: <key> (<used> of
<limit>)`, where `<key>` is the `[budget]` key that was spent.

The kill switch is checked at the same point, and while a run is suspended
on an ask. Its halt reason is `kill switch: SIGINT` or `kill switch: halt
file <path>`.

A `checkpoint` is the last event of its step: it follows the step's final
`budget.tick` and the next event is the `model.request` that opens the next
step, or the `run.state` that halts the run. The trace up to and including
it is the checkpoint. The final step of a finished run has none.

`step` is 0 for the events before the first model request. Each
`model.request` opens the next step.

`tokens_used` counts input, output and cached tokens, read and written.
`usd_used` is the sum of the `price.usd` of every tick so far.

A message blob is `{"role", "content": [block, ...]}`. A block is `text`,
`tool_use`, `tool_result`, or `{"type": "provider", "provider", "block"}`,
where `block` is the provider's own JSON for a block Tether does not model,
kept unchanged.

## Resumed runs

A resumed run appends to its own trace. The resume is marked by a `run.state`
of `running` with the reason `resumed`, whose `step` is the step of the
checkpoint the run went back to (0 if it had none). `seq` stays dense and the
hash chain continues across it.

Because resume goes back to a checkpoint, the events between that checkpoint
and the marker were abandoned: the start of a step, and the `run.state` that
halted it. They stay in the trace, and the step is then recorded again after
the marker with the same `step` number. To read the run as it finally went,
drop everything after the last checkpoint that precedes each marker, up to
but not including the marker. A trace can be resumed more than once.

A crashed run has no closing `run.state`, and may end in a partial line.
Resume drops a last line that has no newline before it appends.

## Size limits

One serialized event is at most 1 MB. Anything larger must have spilled to a
blob; a writer that would exceed the cap fails the run with
`EventTooLarge` rather than writing a bad line.

## Compatibility

Schema v0 is frozen as of 2026-11-21. A trace written by v0.3.0 or any later
release is read by every release after it: `inspect`, `verify`, `diff`,
`replay`, `resume` and `unpack` keep working on it, and its hash chain keeps
verifying, since the chain is over the bytes of each line as written.

What can still change without a new version:

- **A new optional field** in an event's `data`. Readers ignore fields they
  do not know, and a field that is absent reads as its default.
- **A new event type.** A reader that does not know a type carries it as an
  extension event and leaves it out of replay and diff, as it does `ext.*`.
- **The four types no release has written yet:** `run.child`,
  `sandbox.exec`, `sandbox.exit` and `nondeterminism`. Their `data` is not
  frozen until a release writes them.

What needs `v` to go up: removing or renaming a field or a type, changing
what a field means or the type of its value, changing the envelope or its
field order, changing how the hash chain is computed, or changing where blobs
live. When `v` goes up, the reader for `v: 0` stays in `tether-trace`, so
old traces are still read. A build refuses a trace whose `v` is newer than it
knows, by name, and does not guess.

The freeze is held by a test, `crates/tether-trace/tests/compat.rs`. It reads
traces and a pack written by the v0.3.0 release, kept in
`crates/tether-trace/tests/fixtures/`, and checks that they verify with the
same hashes, that each event written back out is the line it was read from,
and that diff and resume still read them the same way. The fixtures are never
regenerated: if the test fails, the change is a breaking one.
