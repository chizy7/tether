# tether

**Run agents you don't trust on infrastructure you do.**

Tether is a model-agnostic, MCP-native runtime that wraps any agent and gives
you the things frameworks leave out: a sandbox, hard budgets, a permission
layer, and a trace of every model call, tool call and policy decision that you
can replay, diff and eval against.

> Status: pre-alpha, v0.2 "it refuses" in progress. `tether run` plays a
> scripted agent under a policy file: rules allow, deny or ask at the terminal,
> budgets halt the run, tools run in the process sandbox, and every decision
> lands in a verifiable trace that `tether inspect` and `tether verify` read.
> Real model adapters and the SDKs are not built yet, so an agent of your own
> has to be Rust code linked against `tether-core`.
> Read [docs/PLAN.md](docs/PLAN.md) for what ships when.

![An agent under Tether tries rm -rf, the policy denies it, and tether inspect shows the decision and the rule that made it](docs/refuses.gif)

The [refuses example](examples/refuses): a scripted agent tries `rm -rf .`,
the `no-rm` rule denies it, and `tether inspect` shows the call, the decision
with the rule that matched, and the failed result the agent got back.

![A wrapped agent runs under Tether, then tether inspect prints its trace and tether verify checks it](docs/demo.gif)

The [raw-loop example](examples/raw-loop) running under Tether with a scripted
model and real sandboxed tools, then `tether inspect` and `tether verify` on
the trace it left. Recorded at v0.1; traces now also carry `budget.tick`
events.

## What it is, and is not

Tether is a **harness**. It runs agents; it does not build them. Bring your
own loop, your own framework, your own prompts. Tether sits beside LangGraph,
the Claude Agent SDK, the OpenAI Agents SDK or a 50-line `while True`, and
owns everything that makes the run safe to leave unattended:

- **Sandboxed tool execution**: `process` (default), `docker`, `firecracker`
- **Policy**: allow / deny / ask rules in TOML, budgets for tokens, dollars, wall clock and tool calls, a kill switch
- **Trace**: one append-only JSONL per run, BLAKE3 hash chain, every decision recorded
- **Replay, diff, eval**: re-run a trace with no network and no cost, diff two runs, score a suite
- **Checkpoint and resume**: a 40-minute task survives a crash
- **OpenTelemetry** out of the box

Not in scope, ever: prompt templates, RAG, memory, agent-builder UIs.

## Integration is two call sites

```python
from tether import Run

with Run(agent="my-agent", policy="tether.toml") as run:
    msgs = [{"role": "user", "content": task}]
    while True:
        resp = run.complete(model="claude-sonnet-4-5", messages=msgs, tools=run.tools())
        if not resp.tool_calls:
            break
        for call in resp.tool_calls:
            msgs.append(run.call_tool(call))   # policy, sandbox, trace all happen here
```

## Layout

```
crates/   nine Rust crates; see CLAUDE.md
proto/    gRPC service for the SDKs
docs/     PLAN, ARCHITECTURE, TRACE_FORMAT
```

```sh
cargo check --workspace --all-targets
cargo test --workspace
```

## Honest note on sandboxing

The default `process` backend runs commands as plain child processes, as the
user running Tether. It is not a security boundary and we will never describe
it as one. What it does is limit the damage of an ordinary mistake:

- Every command starts in the workspace with a cleared environment, and file
  reads and writes are refused outside the workspace.
- A command past its time limit is killed, along with anything it started.
- CPU time and open files are capped with rlimits (Linux and macOS).
- On Linux only: a memory cap, a seccomp filter that refuses a list of system
  calls (tracing other processes, mounting, kernel modules, namespaces,
  setting the clock), no privilege gain through setuid programs, and no
  internet sockets when the run has no network.

On either platform a command can still read and write files outside the
workspace by absolute path. On macOS there is also no memory cap and no
network block. A limit
the platform cannot enforce is refused when the sandbox starts, not ignored;
the exception is the network block on macOS, which is silently absent. Use
the `docker` backend for filesystem and network namespaces (it mounts only
the workspace, and is behind the `docker` cargo feature) and `firecracker`
(v1.0) for a separate kernel.

## License

Apache-2.0.
