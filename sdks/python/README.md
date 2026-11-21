# tether-sdk

Python client for [Tether](../../README.md). Your agent keeps its own loop;
it calls `run.complete` where it called its provider SDK and `run.call_tool`
where it ran a tool, and Tether does the rest: policy, sandbox, budgets and
the trace.

```python
import tether

with tether.Run(policy="tether.toml") as run:
    messages = [tether.user("Write hello.txt, then show me what is in it.")]
    while True:
        reply = run.complete(model="claude-opus-5-5", messages=messages)
        messages.append(reply.message)
        if not reply.tool_calls:
            run.finish(reply.text)
            break
        results = [run.call_tool(call) for call in reply.tool_calls]
        messages.append(tether.tool_results(results))
```

A complete example is in [examples/raw-loop-py](../../examples/raw-loop-py).

## Install

Not on PyPI yet. From a checkout:

```
pip install -e sdks/python
cargo build -p tetherd          # the daemon the SDK talks to
export PATH="$PWD/target/debug:$PATH"
```

## How it works

The SDK is a thin client for the daemon, `tetherd`, over gRPC on a Unix
socket. Each method is one request and nothing in the SDK decides anything.

`tether.Run(...)` connects to the socket and starts a run. If no daemon
answers, it starts one: `$TETHERD` if set, else `tetherd` or `tether daemon`
from `PATH`. The daemon logs to `tetherd.log` beside the socket and exits by
itself a few seconds after its last run.

| | |
| --- | --- |
| `Run(policy=, provider=, script=, dir=, workspace=, agent=, betas=, socket=)` | Start a run. `provider` is `"anthropic"`, `"openai_compat"` (any server with an OpenAI-style `/chat/completions`, set by `OPENAI_BASE_URL` and `OPENAI_API_KEY`) or, with `script`, `"fake"` |
| `run.tools` | The tools the run offers |
| `run.complete(model=, messages=, tools=, params=)` | One model turn. Returns a `Reply` |
| `run.call_tool(call)` | Run a tool the model asked for. Returns a `ToolResult` |
| `run.finish(outcome)` | The agent is done |
| `run.halt()` | Trip the run's kill switch |
| `run.answer(subject_seq, allow)` | Answer a policy ask, from another thread |
| `run.events()` | Iterate the trace as it is written |
| `run.id`, `run.trace_path` | The run's id and where its trace is |

Used as a context manager, a run that leaves its block without a word is
finished, or halted if the block raised.

Things to know:

- Append `reply.message` to your history exactly as it is. It can hold blocks
  only the provider understands, such as reasoning blocks, which must go back
  unchanged.
- Check `reply.stop_reason` before acting. `refusal` and `max_tokens` are
  replies that are not finished answers.
- A tool call that policy denies does not raise. It comes back as a
  `ToolResult` with `is_error` set, like any other failed call.
- `tether.Halted` is raised when the run takes no more calls: a spent budget,
  an unanswered ask, the kill switch, a provider failure. Anything else the
  daemon refuses raises `tether.TetherError` with the gRPC status in `.code`.
- Arguments left unset fall back to `TETHER_POLICY`, `TETHER_DIR` and
  `TETHER_SOCKET`, which `tether run -- python agent.py` sets.

## Develop

```
cd sdks/python
uv venv && uv pip install -e ".[dev]"
cargo build -p tetherd
.venv/bin/python -m pytest
```

The tests start a real daemon from `target/debug/tetherd` and use the
scripted model, so they need no API key. The gRPC stubs in
`src/tether/_proto` are generated and committed; regenerate them after a
change to `proto/tether.proto`:

```
uv run --no-project --with grpcio-tools sh scripts/gen_proto.sh
```
