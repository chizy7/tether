# examples/raw-loop-py

The Python twin of [raw-loop](../raw-loop): a raw agent loop wrapped by
Tether, in one file, `agent.py`.

The agent asks the model, runs the tools it asks for, feeds the results back,
and stops when the model asks for none. Its two call sites go through a
`tether.Run` (`run.complete` and `run.call_tool`) instead of a provider SDK
and a tool dispatcher. That is the whole integration.

## Run it

Build the daemon and install the SDK once, from the repo root:

```
cargo build -p tetherd -p tether-cli
pip install -e sdks/python
export PATH="$PWD/target/debug:$PATH"
```

Then:

```
cd examples/raw-loop-py
python agent.py
tether inspect .tether/run_*.jsonl
```

The SDK starts `tetherd` itself if it is not running, and the daemon exits a
few seconds after the run is over. By default the model is scripted
(`script.json`), so no API key is needed. The tools are real: `write_file`
and `exec` run in the process sandbox, in `.tether/<run>.workspace`.

## With a real model

```
ANTHROPIC_API_KEY=... python agent.py --model anthropic
```

This makes real, billed calls to Claude Opus 5.5; the task is small and costs
a few cents.

```
OPENAI_BASE_URL=http://localhost:11434/v1 python agent.py --model openai_compat <model name>
```

This uses any server with an OpenAI-style chat completions endpoint: a local
one as above, or OpenAI itself with `OPENAI_API_KEY` set and no base URL. The
model needs to support tool calls. Calls are unpriced unless `TETHER_PRICING`
names a pricing table with a row for the model.

Keys and addresses are read by the daemon, so they must be set in the
environment the daemon starts from. If a daemon is already running without
them, wait a few seconds for it to exit, or stop it.

`tether.toml` holds the run to a policy: the file tools and a few read-only
commands are allowed, anything else asks and then halts the run, and budgets
cap it at $0.50, 20 tool calls and 5 minutes.

The request asks for low reasoning effort and turns on the API's server-side
fallback (`fallbacks: "default"`, a beta feature), which retries a request
the model declines on another model in the same call. Remove both from
`agent.py` if you do not want them.
