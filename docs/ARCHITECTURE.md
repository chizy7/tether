# Tether: Architecture

Living copy with diagrams: https://claude.ai/artifact/CbcSFqEjv5hF2yDRebCQmx

Tether is a Rust runtime that wraps any agent: every model call, tool call and
policy decision becomes an event in one append-only trace, and everything else
(replay, diff, eval, checkpoint) is built on that trace.

## Design rules

1. **The trace is the only state.** A run is reconstructable from its JSONL. No crate keeps state that is not also an event; checkpoints index the trace.
2. **Every side effect passes through policy.** Model calls, tool calls, file and network access go through `tether-policy` first; the decision is recorded whether allow, deny or ask.
3. **Every tool is an MCP tool.** The sandbox exposes `exec`, `read_file`, `write_file`, `list_dir` as an MCP server named `sandbox`.
4. **Nondeterminism is captured at the boundary.** Model responses, tool outputs, clock reads and random seeds are recorded on the way in, so replay substitutes them and core logic runs identically.
5. **The core has no opinion about the agent.** No prompt handling, no loop structure, no memory. The agent emits model and tool requests; Tether answers them.

## Run lifecycle

```
Created ──start──▶ Running ──agent done──▶ Finished
                    │  ▲
      policy.ask    │  │ answer: allow
                    ▼  │
                 Suspended ──deny or timeout──▶ Halted
                                                 ▲ │
              budget, kill switch, error ────────┘ │
              tether resume ◀──────────────────────┘ (back to Running)
```

Five states; only Running does work. A step inside Running is one agent turn:
a model request, zero or more tool calls, and the policy decisions around them.
Suspended waits for a human answer. Halted is terminal for this process but
resumable from the trace. Finished is the agent's own exit, never Tether's.
Every transition is a `run.state` event with its reason.

## Trace

See `TRACE_FORMAT.md` for the normative spec. Summary: one JSONL file per run,
one event per line, BLAKE3 hash chain, payloads over 32 KB in a
content-addressed sidecar keyed per message. `tether verify` checks chain and
blobs; `tether pack` bundles both into one tar.

## Policy engine

Rules in `tether.toml`, evaluated top to bottom, first match wins, default
`ask`. Loaded once per run and snapshotted into `run.created.policy` as the
full text of the file, comments included, so a trace says what rules the run
was held to and the policy can be rebuilt from the trace alone.

```rust
pub trait Policy: Send + Sync {
    fn decide(&self, call: &ToolCall, ctx: &RunCtx) -> Verdict;
    fn decide_model(&self, req: &ModelRequest, ctx: &RunCtx) -> Verdict; // default: allow
    fn snapshot(&self) -> Option<String>; // full TOML; default: None
}
pub struct Verdict { decision: Decision, rule: Option<String>, on_timeout: OnTimeout }
pub enum Decision { Allow, Deny { reason: String }, Ask { prompt: String } }
```

`RulePolicy` is the `[[rule]]` tables of `tether.toml` as a `Policy`:

```toml
[[rule]]
name = "no-rm"                                  # optional; default "rule-N" by position
match = { tool = "sandbox.exec", argv0 = "rm" } # every key given must match
action = "deny"                                 # allow | deny | ask
reason = "deleting files is off limits"
```

| `match` key | Matched against |
| --- | --- |
| `tool` | The qualified name `<server>.<tool>`, so the built-in tools are `sandbox.exec`, `sandbox.write_file` and so on |
| `argv0` | The file name of `arguments.argv[0]`: `rm`, `/bin/rm` and `./rm` are all `rm` |
| `path` | `arguments.path` with `.` and `..` resolved; a path that climbs out of where it started matches nothing |

Each key takes one glob or a list. `*` and `?` stay within one path segment,
`**` crosses `/`. A key left out matches anything, so a rule with no `match`
is a catch-all. The matched rule's name is recorded in `policy.decision.rule`;
a call no rule matches is an `ask` with `rule` null. Unknown keys, duplicate
names and empty patterns are load errors, so a typo cannot quietly widen a
rule.

Rules match what a call says, not what it will do: `argv0 = "rm"` does not see
`sh -c "rm -rf ."`. Deny by default and allow narrowly where that matters.

`Ask` suspends the run and emits `policy.ask`. The answer comes from the CLI
prompt, the SDK `Answer` RPC, or, unattended, a timeout (default 600 s) that
moves the run to Halted, resumable once someone answers. A rule may set
`on_timeout = "deny"` for fire-and-forget jobs.

```rust
#[async_trait]
pub trait Answerer: Send + Sync {
    async fn answer(&self, ask: &AskRequest) -> AskOutcome; // Answered { allow, answered_by } | Unanswered { waited_secs }
}
```

The run hands each ask to its `Answerer` and waits. The answerer owns the
waiting, timeout included, so the core never touches a timer and replay can
substitute the recorded `policy.answer`. A yes dispatches the call. A no gives
the agent an ordinary failed tool call and the run goes on; for a model call
there is nothing to hand back, so a no halts. `ask_timeout_secs` in `[budget]`
sets the wait. `Unattended` is the answerer for a run nobody is watching:
every ask goes unanswered at once.

`tether run --script <file> --policy <tether.toml>` uses the terminal: the
prompt goes to stderr and `y` or `n` is read from stdin. Closed stdin counts
as nobody there. Until the SDK boundary exists, `tether run` plays a scripted
agent: the script gives the task and the model's replies, and the tool calls,
policy, sandbox and trace are real.

Budgets come from the `[budget]` table: `max_tokens`, `max_usd`,
`max_wall_secs` and `max_tool_calls`, each optional. All four are checked
before every model and tool call, ahead of the call's first event, and a
spent one halts the run with a `run.state` reason naming its key. A budget is
spent when what has been used reaches the limit, so `max_tool_calls = 400`
allows exactly 400 calls. Tokens, dollars and wall time are only known once a
call returns, so the call that crosses one of those completes and the next is
refused; a long-running tool is not interrupted. Only dispatched tool calls
count, not denied ones. A `budget.tick` follows every call.

The kill switch stops a run from outside. Two things trip it: SIGINT (Ctrl-C
in `tether run`) and a halt file, `<run>.halt` beside the trace, which trips
it by existing (`touch run_123.halt`). Tripping only sets a flag. The run
loop looks at it ahead of every model and tool call and while it waits on an
`ask`, and halts there with a `run.state` reason of `kill switch: SIGINT` or
`kill switch: halt file <path>`. A run is therefore never stopped in the
middle of writing an event. A call already in flight is not interrupted: it
finishes, its result is recorded, and the run halts at the boundary after it.
A second Ctrl-C exits at once, for a run stuck inside a long call. The halt
file is left in place; remove it before resuming.

The engine decides; it does not inspect model output for intent and it does
not sandbox. The sandbox enforces.

## Sandbox

```rust
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn start(&self, spec: &SandboxSpec) -> Result<Handle>;
    async fn exec(&self, h: &Handle, cmd: Exec) -> Result<ExecResult>;
    async fn read(&self, h: &Handle, path: &Path) -> Result<Vec<u8>>;
    async fn write(&self, h: &Handle, path: &Path, data: Vec<u8>) -> Result<()>;
    async fn snapshot(&self, h: &Handle) -> Result<SnapshotId>;
    async fn stop(&self, h: Handle) -> Result<()>;
}
```

| Backend | Phase | Isolates | Does not isolate |
| --- | --- | --- | --- |
| `process` | v0.1 | cwd jail, env scrub, time limit, rlimits; on Linux also a memory cap, a seccomp deny list and no internet sockets under `Network::None` | Kernel, filesystem outside the jail by absolute path, network on macOS. **Not a security boundary.** |
| `docker` | v0.2 | Filesystem, network namespace, resource limits; snapshot via `commit` | Kernel (shared) |
| `firecracker` | v1.0 | Kernel, memory, devices; VM snapshot | Side channels, hypervisor bugs |

What `process` does, by platform:

| Setting | Linux | macOS |
| --- | --- | --- |
| Workspace cwd, env scrub, `read`/`write` path check | yes | yes |
| `time_limit_secs`: kills the command and its process group | yes | yes |
| `cpu_time_secs` (`RLIMIT_CPU`), `max_open_files` (`RLIMIT_NOFILE`) | yes | yes |
| `memory_bytes` (`RLIMIT_AS`) | yes | refused at `start` |
| seccomp deny list, `no_new_privs` | yes | no |
| `Network::None` | internet sockets refused | not enforced |
| `Network::Allowlist` | refused at `start` | refused at `start` |

The seccomp filter is a deny list returning `EPERM`: tracing and reading
other processes, mounts and `chroot`, namespaces, kernel modules, `reboot`,
`bpf`, `perf_event_open`, keyrings, setting the clock, and `io_uring`.
Everything else is allowed, so ordinary build and test commands run
unchanged. `memory_bytes` caps address space, not resident memory, so
runtimes that reserve large virtual ranges (Go, the JVM, Node) need a
generous value. `cpu_millis` is a CPU share for the container backends and
`process` ignores it. A limit the platform cannot enforce is refused at
`start` rather than ignored, with the one exception in the table.

Backends that cannot snapshot return `Unsupported`; resume then replays writes
from the trace into a fresh sandbox.

## Tool layer

`tether-mcp` connects every configured MCP server at run start, calls
`tools/list`, merges the result (collisions prefixed by server id) and records
it in `run.created`. The built-in `sandbox` server is one of them. Routing for
every call: `tool.call` event, `Policy::decide`, dispatch, `tool.result`. A
deny produces a `tool.result` with `is_error = true` so the agent sees a normal
failed call. Each release pins one MCP protocol version. Tether ships no tools
beyond the sandbox server.

Client: `rmcp` behind the `McpClient` trait under a cargo feature; a minimal
hand-rolled client is the fallback if `rmcp` blocks stdio or streamable HTTP.

## Model adapters

```rust
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn id(&self) -> &str;
    async fn complete(&self, req: CompletionRequest) -> Result<Completion>;
    fn price(&self, usage: &Usage) -> Option<Money>;
}
```

| Adapter | Phase | Notes |
| --- | --- | --- |
| `anthropic` | v0.1 | Messages API, tool use; streaming collapsed to one event with `ttft_ms` |
| `openai_compat` | v0.3 | Chat Completions shape |
| `ollama` | after v0.4 | Local; `price` returns `None` |
| `replay` | v0.3 | Reads `model.response` events from a trace |
| `fake` | v0.1 | Scripted responses for tests; no API key |

Adapters see message content in full and know nothing about the trace:
`CompletionRequest` and `Completion` carry real messages, and `tether-core`
turns them into `model.request` and `model.response` events by storing each
message as a blob. Latency is measured by the core through `Clock`.

Tether never modifies prompts. `model.request` records Tether's form;
`model.response` records Tether's form plus the raw provider body as a blob.
Pricing tables are versioned TOML; the price used is recorded in `budget.tick`.

## Checkpoint, resume, replay

All three re-run the core loop while feeding it recorded answers.

- **Checkpoint:** every N steps (default 1) a `checkpoint` event with the step and, if supported, a sandbox `SnapshotId`. The trace up to that seq is the checkpoint.
- **Resume:** read to the last checkpoint, restore the snapshot (or replay writes into a fresh sandbox), rebuild message history from `model.*` and `tool.*` events, continue live with `run.state = resumed`.
- **Replay:** run the whole trace with the `replay` adapter and a replay sandbox; requests matched to recorded responses by content hash; no network, no cost. Policy is re-evaluated live, which is how a policy change is tested against old runs.
- **Diff:** align two traces by step and report where model requests, tool calls or policy decisions diverge.

Where determinism breaks: a changed prompt stops replay at the divergence
(or goes live with `--live-after-divergence`); tools marked `external` return
the recorded result and are never re-invoked; clock, random and env go through
`Clock` and `Rng` traits satisfied from `nondeterminism` events; agent-side
nondeterminism is out of scope and shows up in diff as the agent's own change.

## SDK boundary

One gRPC service (`proto/tether.proto`) on a Unix socket; Python and
TypeScript SDKs are thin clients. The agent replaces two call sites: its
provider SDK call becomes `Complete`, its tool dispatch becomes `CallTool`.
`tetherd` starts on demand and exits after its last run.

## Crate graph

```
tether-cli   tetherd
     \         /
     tether-core
   /   |   |   |   \
policy sandbox mcp models trace
   \   |   |   |   /
     tether-types
```

Dependencies point down only. Leaf crates never import each other or
`tether-core`; `tether-cli` also reads traces directly via `tether-trace` for
`inspect`, `diff` and `verify` without a daemon.

## Decisions

| Question | Decision | Escape hatch |
| --- | --- | --- |
| Streaming | One collapsed `model.response` with `stream: true`, `ttft_ms` | `--record-deltas` writes raw deltas to a blob |
| Blob storage | Hybrid: inline under 32 KB, content-addressed sidecar per message | `tether pack` for one-file portability |
| Multi-agent | One trace per run, `agent` field, single writer task | Separate process gets its own run id plus `run.child` |
| Unattended `ask` | 600 s timeout, then Halted (resumable) | `on_timeout = "deny"` per rule |
| MCP client | `rmcp` behind the trait, cargo feature | `minimal` hand-rolled client |
| Hash chain | Keep; BLAKE3 in the writer task | `--no-chain` for throwaway runs; `tether verify` reports it |
