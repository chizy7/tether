# Tether trace format, schema v0

Status: draft, frozen at v0.3. Changes before then go through `tether-types`
and this file in the same PR.

## Files

```
<run>.jsonl                          one Event per line, append-only, UTF-8
<run>.blobs/blake3/<ab>/<full hex>   content-addressed payloads over 32 KB
```

`tether pack <run>` bundles both into one tar. All tools accept either form.

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
| `run.created` | agent id, model config, `policy` (the full text of the policy TOML as one string, byte for byte; null when the policy did not come from a file), `sandbox` (`{"backend": "process"}` or the like; null when the run has no sandbox), merged tool list, schema version |
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

## Size limits

One serialized event is at most 1 MB. Anything larger must have spilled to a
blob; a writer that would exceed the cap fails the run with
`EventTooLarge` rather than writing a bad line.
