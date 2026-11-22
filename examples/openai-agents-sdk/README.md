# examples/openai-agents-sdk

An [OpenAI Agents SDK](https://openai.github.io/openai-agents-python/) agent
wrapped by Tether, in one file, `agent.py`.

The loop is the framework's. `Runner.run` decides when to call the model and
when to run a tool, and Tether takes its two call sites:

- `TetherModel` is the agent's model. Each model turn is one `run.complete`.
- `tether_tools` are the agent's tools. Each tool call is one `run.call_tool`.

That is about forty lines, and it is the whole integration. Policy, the
sandbox, budgets and the trace follow from those two calls, the same as in
the [raw loop](../raw-loop-py). Run with the same script, this agent and the
raw loop leave traces that `tether diff` finds no divergence between.

## Run it

From the repo root, once:

```
cargo build -p tetherd -p tether-cli
pip install -e sdks/python
pip install -r examples/openai-agents-sdk/requirements.txt
export PATH="$PWD/target/debug:$PATH"
```

Then:

```
cd examples/openai-agents-sdk
python agent.py
tether inspect .tether/run_*.jsonl
```

By default the model is scripted (`script.json`), so no API key is needed,
not an OpenAI one either: the framework never talks to a model itself.

## With a real model

```
ANTHROPIC_API_KEY=... python agent.py --model anthropic
```

The model is whichever one Tether's adapter reaches, so an Agents SDK agent
can run on Claude this way. The key is read by the daemon, so it must be set
in the environment the daemon starts from.

## Things to know

- **The history is kept in Tether's form.** The framework hands the model its
  conversation as OpenAI Responses items. `TetherModel` reads only what is
  new in it (tool results, further user messages) and keeps its own list of
  messages, so each reply goes back to the provider exactly as it came,
  reasoning blocks included.
- **No streaming.** `Runner.run_streamed` is not supported: Tether returns
  each model turn whole.
- **The framework's own tools and handoffs are not wrapped.** Hosted tools,
  handoffs and guardrails that call a model outside `TetherModel` would not
  pass through Tether. This example uses the run's sandbox tools only.
- **A halt inside a tool arrives wrapped.** The framework catches a tool's
  exception and raises its own, with `tether.Halted` as the cause. `main`
  unwraps it.
- **The framework's tracing is turned off.** It uploads to OpenAI by default.
  The trace here is Tether's.

`requirements.txt` pins the framework version this was written against
(0.25.0). Its `Model` interface is what the example depends on.
