# examples/raw-loop-ts

The TypeScript twin of [raw-loop](../raw-loop): a raw agent loop wrapped by
Tether, in one file, `agent.ts`.

The agent asks the model, runs the tools it asks for, feeds the results back,
and stops when the model asks for none. Its two call sites go through a
Tether `Run` (`run.complete` and `run.callTool`) instead of a provider SDK
and a tool dispatcher. That is the whole integration.

## Run it

Build the daemon and the SDK once, from the repo root:

```
cargo build -p tetherd -p tether-cli
(cd sdks/typescript && npm install && npm run build)
export PATH="$PWD/target/debug:$PATH"
```

Then:

```
cd examples/raw-loop-ts
npm install
npm start
tether inspect .tether/run_*.jsonl
```

The example uses the SDK from this checkout. Outside the repository, depend
on `tether-agents` from npm instead.

The SDK starts `tetherd` itself if it is not running, and the daemon exits a
few seconds after the run is over. By default the model is scripted
(`script.json`), so no API key is needed. The tools are real: `write_file`
and `exec` run in the process sandbox, in `.tether/<run>.workspace`.

## With a real model

```
ANTHROPIC_API_KEY=... npm start -- --model anthropic
```

This makes real, billed calls to Claude Opus 5.5; the task is small and costs
a few cents.

```
OPENAI_BASE_URL=http://localhost:11434/v1 npm start -- --model openai_compat <model name>
```

This uses any server with an OpenAI-style chat completions endpoint. Keys and
addresses are read by the daemon, so they must be set in the environment the
daemon starts from.

`tether.toml` holds the run to a policy: the file tools and a few read-only
commands are allowed, anything else asks and then halts the run, and budgets
cap it at $0.50, 20 tool calls and 5 minutes.
