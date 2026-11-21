# Trace fixtures from v0.3.0

These traces were written by the released v0.3.0 `tether` binary and the
`tether-agents` 0.3.0 Python package, on 2026-11-21. `tests/compat.rs` reads
them to prove that later builds still read what v0.3.0 wrote.

Do not regenerate or edit them. A fixture made by a newer build proves
nothing about old traces. When a release first writes a new event type, add a
new fixture made by that release and leave these alone.

| Fixture | How it was made |
| --- | --- |
| `refuses` | `tether run --script` on `examples/refuses`: a denied `rm -rf` |
| `resumed` | A scripted run halted by an ask nobody answered, then `tether resume` with a yes |
| `spilled` | A scripted run whose tool result is over the inline limit, halted by `max_tool_calls` |
| `sdk` | `examples/raw-loop-py` through the daemon with `provider="openai_compat"`, against a local stand-in server, priced by a `TETHER_PRICING` table |
| `sdk_replay` | `tether replay` of `sdk` |
| `refuses.tar` | `tether pack` of `refuses` |

No fixture comes from a real model: none of this needs a key. The `sdk`
trace has the shape of one, with a streamed response, a raw provider body and
a price.
