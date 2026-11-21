# examples/raw-loop

A raw agent loop wrapped by Tether, in one file: `main.rs`.

The agent asks the model, runs the tools it asks for, feeds the results back,
and stops when the model asks for none. Its two call sites go through a
`tether_core::Run` (`run.complete` and `run.call_tool`) instead of a provider
SDK and a tool dispatcher. That is the whole integration.

By default the model is the scripted `fake` adapter, so no API key is needed.
The tools are real: `write_file` and `exec` run in the process sandbox, in
`target/raw-loop/workspace`.

```
cargo run -p tether-core --example raw-loop
cargo run -p tether-cli -- inspect target/raw-loop/<run>.jsonl
cargo run -p tether-cli -- verify target/raw-loop/<run>.jsonl
```

## With a real model

`--model anthropic` swaps the scripted model for Claude Opus 5.5 through the
Anthropic adapter. The agent loop and the tools do not change. This makes
real, billed API calls; the task is small and costs a few cents.

```
ANTHROPIC_API_KEY=... cargo run -p tether-core --example raw-loop -- --model anthropic
```

A real model can ask for anything, so this mode runs under a policy instead
of allowing everything: the file tools and a few read-only commands (`cat`,
`ls`, `head`, `wc`, `pwd`, `echo`) are allowed and anything else asks. Nobody
is attached to answer, so an ask halts the run. Budgets cap it at $0.50, 20
tool calls and 5 minutes.

The request asks for low reasoning effort and turns on the API's server-side
fallback (`fallbacks: "default"`, a beta feature), which retries a request
the model declines on another model in the same call. Remove both lines from
`anthropic()` in `main.rs` if you do not want them.

In the trace, each `model.response` has real token counts, a `stop_reason`,
time to first token and the raw event stream as a blob, and the
`budget.tick` after it has the price of that call.

The example is in-process because the SDK boundary (`tetherd`, gRPC) lands in
v0.3. Until then an agent has to be Rust code linked against `tether-core`.
