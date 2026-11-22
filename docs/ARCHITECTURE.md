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
| `tool` | The qualified name `<server>.<tool>`, so the built-in tools are `sandbox.exec`, `sandbox.write_file` and so on. Matched as a name: `*` covers a `/` too |
| `argv0` | The file name of `arguments.argv[0]`: `rm`, `/bin/rm` and `./rm` are all `rm` |
| `path` | `arguments.path` with `.` and `..` resolved. A path that climbs out of where it started matches every `deny` and `ask` with a `path`, and no `allow` |
| `server` | The id of the server that owns the tool, matched as a name |

Each key takes one glob or a list. `*` and `?` stay within one path segment,
`**` crosses `/`. A key left out matches anything, so a rule with no `match`
is a catch-all. The matched rule's name is recorded in `policy.decision.rule`;
a call no rule matches is an `ask` with `rule` null. Unknown keys, duplicate
names, empty patterns and patterns over 1024 characters are load errors, so a
typo cannot quietly widen a rule.

A `deny` or an `ask` matches `argv0` and `path` without regard to letter
case, because many filesystems resolve names that way and `SECRETS/` would
otherwise walk past a deny on `secrets/**`. An `allow` matches as written.
Matching is a table over pattern and text, linear in each, with no
backtracking: the text is the agent's and the pattern may come from a trace.

The built-in tools take their arguments as a JSON object and nothing else.
A list would be read into the same fields by position, with no `argv` or
`path` for a rule to match on.

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
    fn backend(&self) -> &'static str; // "process", "docker"
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
| `docker` | v0.2 | Filesystem (only the workspace is mounted), network namespace, resource limits; snapshot via `commit` | Kernel (shared) |
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
other processes, mounts and `chroot`, `unshare` and `setns` (namespaces made
through `clone` flags are not covered), kernel modules, `reboot`,
`bpf`, `perf_event_open`, keyrings, setting the clock, and `io_uring`.
Everything else is allowed, so ordinary build and test commands run
unchanged. `memory_bytes` caps address space, not resident memory, so
runtimes that reserve large virtual ranges (Go, the JVM, Node) need a
generous value. `cpu_millis` is a CPU share for the container backends and
`process` ignores it. A limit the platform cannot enforce is refused at
`start` rather than ignored, with the one exception in the table.

`docker` (cargo feature `docker`) keeps one long-lived container per sandbox
and drives it through the `docker` command line, so it adds no dependency.

| Setting | What the container gets |
| --- | --- |
| `workspace` | Bind-mounted at `/workspace`, the working directory and the only host path visible |
| `Network::None` / `All` | `--network none` (loopback only) / the default bridge |
| `Network::Allowlist` | Refused at `start` |
| `cpu_millis`, `memory_bytes` | `--cpus`, `--memory` with no swap |
| `cpu_time_secs`, `max_open_files` | `--ulimit cpu`, `--ulimit nofile` |
| `time_limit_secs` | `timeout -s KILL` inside the container, so the kill reaches the command |

Commands run as the host user, so files written to the workspace belong to
whoever runs Tether, with every capability dropped and `no-new-privileges`
set. `read` and `write` go through the container rather than the host's
filesystem: a symlink an agent plants in the workspace resolves inside the
container and cannot point the host at its own files. `snapshot` commits the
container to an image and returns the image id; it holds the container's own
filesystem, not the workspace, which is a mount and stays on the host.
`DockerSandbox::with_image` starts from a tag or a snapshot id. The image
needs `sh`, `sleep`, `timeout`, `cat`, `mkdir` and `dirname`; the default is
`debian:stable-slim`.

Backends that cannot snapshot return `Unsupported`; resume then replays writes
from the trace into a fresh sandbox.

## Tool layer

`tether-mcp` defines one `McpClient` trait (`list_tools`, `call`, `servers`,
`shutdown`) and three implementations: the built-in `sandbox` server
(in-process, `exec`, `read_file`, `write_file`, `list_dir`), `ExternalServer`
(an MCP server over stdio or streamable HTTP, through `rmcp`), and `Registry`,
which holds any number of the other two and is what the run talks to.

At run start every `[[mcp_server]]` in the policy is connected: `initialize`,
a check that the negotiated protocol version is one this build supports,
`tools/list`. The registry merges the lists, built-in server first; a name
that collides is exposed as `<server>__<tool>` for the later server, so the
sandbox keeps its bare names. The merged list goes into `run.created.tools`
with the owning server on each entry, and `run.created.mcp_servers` records
each server's transport, negotiated protocol version and reported name.

Routing for every call: `tool.call` event (with `server` filled in), policy
decision, dispatch to the owning server under its own name for the tool,
`tool.result`. A deny produces a `tool.result` with `is_error = true`. A call
past the server's `timeout_secs`, a JSON-RPC error, and a tool the server
does not have all come back the same way; none of them halts the run. Rules
see every tool as `<server>.<tool>`, whichever name the agent called it by,
and may match on `server` alone.

Stdio servers are child processes with a scrubbed environment: `PATH`, so
the server can find its interpreter, and the variables the policy's `env`
list names. Nothing else reaches them. They stop when the run finishes or
halts. `timeout_secs` bounds the `initialize` handshake as well as each call,
so a server that never answers fails the run at start and does not hang it.

An HTTP server's `headers` are sent with every request. The policy file is
recorded whole in `run.created.policy`, so a token must not be written in
it: a header value takes `${NAME}`, filled in from Tether's environment when
the server is connected, and a name that is not set fails the connect.

Every external tool is `external = true`, so replay returns the recorded
result and connects to nothing; `tether replay` of a trace with external
calls starts no server. The paths that go live are different. `tether resume`
connects the servers the recorded policy names before it carries on, and a
replay with `--live-after-divergence` (so `tether eval --system` too)
connects them at the moment it leaves the recording, not before. Neither
makes an external call a second time.

Those servers come out of the trace, and a trace can come from anyone: its
recorded policy could name any command. So the live paths start a server
only if it is in a `--policy` file the user passed, or the user passed
`--trust-recorded-servers`, and they print what each server runs or
connects to first. Otherwise `resume` refuses, and a live replay or eval
carries on without them: a call to one of their tools comes back as a
failed call that says why.

The client is `rmcp` 3.11 (`client`, `transport-child-process`,
`transport-streamable-http-client-reqwest`). It sits behind the `rmcp` cargo
feature, on by default; a build without it refuses any `[[mcp_server]]` with a clear
error. Tether ships no tools beyond the sandbox server.

## Model adapters

```rust
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn id(&self) -> &str;
    async fn complete(&self, req: CompletionRequest) -> Result<Completion>;
    fn price(&self, model: &str, usage: &Usage) -> Option<Price>; // usd plus pricing table version
}
```

| Adapter | Phase | Notes |
| --- | --- | --- |
| `anthropic` | v0.3 | Messages API over HTTP, tool use; streaming collapsed to one event with `ttft_ms`; key from `ANTHROPIC_API_KEY` |
| `openai_compat` | v0.3 | Chat Completions shape over HTTP, for OpenAI and the servers that copy its endpoint; server from `OPENAI_BASE_URL`, key from `OPENAI_API_KEY`; cargo feature `openai_compat` |
| replay | v0.3 | Not in `tether-models`: the stand-in built by `tether-core::replay`, which returns recorded `model.response` events |
| `fake` | v0.1 | Scripted responses for tests; no API key |

Adapters see message content in full and know nothing about the trace:
`CompletionRequest` and `Completion` carry real messages, and `tether-core`
turns them into `model.request` and `model.response` events by storing each
message as a blob. Latency is measured by the core through `Clock`.

Tether never modifies prompts. `model.request` records Tether's form;
`model.response` records Tether's form plus the raw provider body as a blob.

A message is a role and a list of blocks: `text`, `tool_use`, `tool_result`,
and `provider`. A `provider` block is one only its provider understands,
kept as the provider's own JSON: reasoning blocks with their signatures,
server tool results, fallback markers. An adapter sends such a block back
unchanged to the provider that made it and leaves it out for any other. This
matters because the Anthropic API rejects a conversation whose earlier
reasoning blocks were altered or dropped from the middle, so an agent should
append each reply to its history exactly as it received it.

A `Completion` also carries `stop_reason` in the provider's words and the
`model` that answered. An agent should read `stop_reason` before acting:
`refusal` and `max_tokens` are successful responses that are not finished
answers. `model` can differ from the one asked for after a provider-side
fallback, and the call is priced as the model that answered.

The `anthropic` adapter:

- **Request.** `params` passes through as the body's other fields
  (`max_tokens`, `thinking`, `output_config`, `fallbacks` and so on), so the
  agent decides them. The adapter adds only `max_tokens` when none is given.
  Leading system messages become the `system` prompt; a `tool` message
  becomes a user turn of `tool_result` blocks. Beta headers are set on the
  adapter with `.beta(...)`.
- **Response.** Streamed by default. The event stream is collapsed into the
  message a non-streaming call would have returned, `raw` is the whole stream,
  and `ttft_ms` is the time to the first content. An event type the adapter
  does not know is skipped; a content delta it does not know is an error,
  because guessing would corrupt a block that must go back unchanged.
- **Errors.** A rate limit, overload or server error is retried twice with
  backoff, honouring `retry-after`; anything else is a `ModelError::Api` with
  the status, error type, message and request id. Retries happen inside one
  `model.request`, so the trace shows them only as latency. Nothing is
  retried once a response has started.
- **Version.** Each release pins one API version (`2026-10-21`), which no
  longer accepts `temperature`, `top_p` or `top_k`.

The `openai_compat` adapter talks to any server with an OpenAI-style
`/chat/completions`: OpenAI, a hosted gateway, or a local server. It is
behind the `openai_compat` cargo feature of `tether-models`, which the
`tether` and `tetherd` binaries turn on by default.

- **Request.** `params` passes through as the body's other fields. The
  adapter adds only `stream` and, when streaming, `stream_options` asking for
  usage in the last chunk. System messages stay in the conversation as
  `system` messages. An assistant turn carries its `tool_use` blocks as
  `tool_calls`, with arguments as JSON text. Each `tool_result` block becomes
  a `tool` message of its own.
- **Response.** Only the first choice is read. Streamed by default and
  collapsed into the body a non-streaming call would have returned.
  `stop_reason` is the choice's `finish_reason`, so it reads `stop`,
  `tool_calls`, `length` or `content_filter` here, not Anthropic's words.
  Tokens read from the cache are counted inside `prompt_tokens` on the wire;
  the adapter takes them out of `input_tokens` and reports them as
  `cache_read_tokens`, the way every other adapter does.
- **What the shape cannot carry.** A tool result has no error flag, so
  `is_error` is not sent. Reasoning text that some servers return beside the
  answer is not put in the message, because servers disagree on whether it
  may be sent back; it is still in the raw body. A refusal is kept as a
  `provider` block and goes back in the message's `refusal` field.
- **Errors.** As for `anthropic`: a rate limit or server error is retried
  twice with backoff, anything else is a `ModelError::Api`.
- **Credentials.** `OPENAI_BASE_URL` is the address `/chat/completions` is
  appended to and defaults to OpenAI's. `OPENAI_API_KEY` is sent as a bearer
  token; it may be left out for a server that needs none.

Prices live in `crates/tether-models/pricing.toml`, dollars per million
tokens, with a `version`. The cost of each model call and that version are
recorded in the `budget.tick` after it. A model missing from the table has no
price: its calls do not count toward `max_usd` and its ticks carry no price.
The built-in table has rows for Anthropic models only, so calls through
`openai_compat` are unpriced until a table is given. `Pricing::from_toml`
loads a different table, and the adapters' `from_env` reads one from the file
named in `TETHER_PRICING`; it replaces the built-in table whole. Two things are under-counted:
one-hour cache writes are priced as five-minute ones, and Claude Haiku 5.5's
higher rate for prompts over 100K tokens is not modelled.

Tool names go to the provider as they are. The APIs accept only letters,
digits, `_` and `-`, which is why the registry prefixes a colliding name with
two underscores (`github__create_issue`) and not a dot. A server whose own
tool names use other characters is not renamed, and the provider will reject
the request.

## Checkpoint, resume, replay

All three re-run the core loop while feeding it recorded answers.

- **Checkpoint:** every N steps (default 1) a `checkpoint` event with the step and, if supported, a sandbox `SnapshotId`. The trace up to that seq is the checkpoint. See "Checkpoints" below.
- **Resume:** go back to the last checkpoint, restore the snapshot (or replay writes into a fresh sandbox), rebuild message history from `model.*` and `tool.*` events, and continue live in the same trace after a `run.state` of `running` with reason `resumed`. See "Resume" below.
- **Replay:** run a trace again, answered from the recording: requests matched to recorded responses by message hash, recorded tool results and human answers given back, timings restored; no network, no cost. Policy is re-evaluated live, which is how a policy change is tested against old runs. See "Replay" below.
- **Diff:** align two traces by step and report where model requests, tool calls or policy decisions diverge. See "Diff" below.

Where determinism breaks: a changed prompt stops replay at the divergence
(or goes live with `--live-after-divergence`); tools marked `external` return
the recorded result and are never re-invoked; clock, random and env go through
`Clock` and `Rng` traits satisfied from `nondeterminism` events; agent-side
nondeterminism is out of scope and shows up in diff as the agent's own change.

### Replay

`tether-core::replay` answers a run from a recording. `Recording::load` reads
and verifies a trace; `Recording::parts` builds stand-ins for everything a
run asks of the outside:

| Stand-in | Gives back | Matched on |
| --- | --- | --- |
| model | the recorded `model.response`, its price, or the provider's recorded error | the request's message hashes |
| tools | the recorded `tool.result`, whole even if it had spilled to a blob | tool name and arguments |
| answerer | the recorded `policy.answer`, in order | order |
| timeline | `ts`, `latency_ms`, `duration_ms`, `wall_secs_used`, checkpoint snapshot ids | the same kind of event within the same call |

Policy is the one thing not played back. The replay evaluates the recorded
policy file again, or another one, so a changed policy can be tried against
an old run. A trace with no policy file plays back its recorded decisions.

A replay **diverges** the first time it is asked something the recording
cannot answer: a model request whose messages hash differently, a tool call
it has no result for (including a call the recording denied and so never
ran), or an ask it has no answer to. From then on each stand-in hands over
to its live counterpart if it was given one (`Live`), and fails if not.
Before the live tools run their first call they are brought up to date by
running again the calls that had been replayed; tools marked `external` are
never run again. A stricter policy is not a divergence: denying a call needs
nothing the recording lacks, so the replay runs to the end and `tether diff`
shows which decisions changed.

Halts that came from outside the run are replayed from their recorded
reason, since nothing else in the trace says they happened: an unanswered
ask (`ask unanswered after N s`), the kill switch (`kill switch: ...`) and a
provider error (`model error: ...`).

The core only answers. Whatever asks is outside it, in keeping with design
rule 5: either the original agent run again, or `tether replay`, which
re-issues the recorded requests and tool calls in order.

```
tether replay <run.jsonl> [--out <path>] [--policy <tether.toml>]
              [--system <text>] [--live-after-divergence [--script <file>]]
```

It writes a new trace, `<run>.replay.jsonl` by default, with its own run id.
`tether diff` of a trace and its replay is empty, and stricter than that:
every event is equal but for the run id and the hash chain. `--system`
replaces the system prompt of each replayed request, which diverges at the
first one. `--live-after-divergence` then carries on as a plain tool loop
against the live model (the recorded provider, or `--script` to stand in for
it) and real tools in a fresh workspace beside the new trace, never the
recorded run's. Without it the replay stops where it diverged and exits 1.
Request headers are not in a trace, so the one beta header the Anthropic
adapter needs for a recorded `fallbacks` field is added back by name.

### Checkpoints

A step is complete when the agent asks for its next completion, so that is
when its checkpoint is written: after the step's last `budget.tick` and
before the next `model.request`. It is written ahead of the kill switch and
budget checks, so a run halted between steps still has the step it finished.
A run halted inside a step resumes from the checkpoint before it. The last
step of a finished run gets no checkpoint, because nothing resumes a finished
run.

`RunConfig::checkpoint_every` sets N; 0 writes none. `snapshot` is the
sandbox's own state when the backend can capture it: an image id from
`docker`, null from `process`, and null when a snapshot fails, which is
logged and does not stop the run. With a null snapshot, resume rebuilds the
sandbox by replaying the trace's writes.

Each `docker` snapshot is a committed image, one per checkpoint, labelled
`tether=snapshot`. They are not cleaned up; remove them with
`docker image prune --all --filter label=tether=snapshot`, or raise N.

The run loop reaches the sandbox through `RunDeps::sandbox`, which also puts
the backend's name in `run.created.sandbox`.

### Resume

`tether resume <run.jsonl> [--script <file>]` carries on a run that halted or
crashed. It appends to the same trace; nothing already written changes, and
the hash chain runs unbroken across the resume.

1. **Repair.** A crash can leave half an event at the end of the file. A last
   line with no newline was never a complete event and is dropped. The trace
   is then verified, and a trace that fails is not appended to.
2. **Go back.** The run returns to its last checkpoint, or to its start if it
   has none. Budgets used so far come from the trace at that point; limits
   come from the policy. Elapsed time carries on from the checkpoint, so time
   spent halted is not counted.
3. **Rebuild the sandbox.** With a null snapshot the tool calls that ran
   before the checkpoint are run again, in order, in a fresh workspace
   `<run>.resume-<n>.workspace`. Tools marked `external` are not run again.
   The halted run's own workspace is left as it was.
4. **Mark.** `run.state` `running`, reason `resumed`, in the checkpoint's step.
5. **Carry on.** The step that was under way is done again from its
   `model.request`, whose messages are read back from the blob store. If the
   run halted between steps, the next request is built from the finished
   step's request, reply and tool results.

The step that was under way therefore appears in the trace twice: once
abandoned, once done. `tether_trace::effective` gives the events of the run
as it finally went, leaving out the stretch between each resume's checkpoint
and its marker. `tether diff` compares that, so a resumed run and one that
was never interrupted do not diverge.

`Run::resume` in the core does steps 2 and 4 and reopens the trace;
`Recording::resume_plan` works out the rest. As with replay, what asks is
outside the core: with no agent attached, `tether resume` is a plain tool
loop.

Limits today:

- The run is held to the policy recorded in its trace, and there is no way to
  resume under another. A run halted by a spent budget halts again at once.
  A trace with no policy file cannot be resumed by the CLI.
- Only the `process` sandbox, so only the replay-the-calls path. Restoring a
  `docker` snapshot is not wired into the CLI.
- A `<run>.halt` file must be removed first, or the run would halt again.
- A resumed trace cannot be replayed yet.
- Doing a step again costs its model call again, and the model may answer
  differently from the abandoned attempt.

### Pack

`tether pack <run.jsonl>` writes `<run>.tar`: the trace and the blobs it
refers to, in one plain, uncompressed tar that any tar tool can read.
`tether unpack <run.tar>` turns it back into the two.

- The trace is verified before it is packed and after it is unpacked. A
  trace that fails is not packed, and an unpack that fails leaves nothing
  behind.
- Only blobs the trace refers to go in. Anything else in the blob store is
  left out.
- Packing the same trace twice gives the same bytes: entries are in a fixed
  order with no times or owners.
- Unpack accepts exactly two kinds of entry, one `<run>.jsonl` and blobs
  under `<run>.blobs/blake3/`, and checks every path before writing
  anything. A tar with any other entry, or a path that would land outside
  the target directory, is refused. Nothing is overwritten.

`inspect`, `verify`, `diff` and `replay` take a `.tar` wherever they take a
`.jsonl`. They unpack it into a scratch directory, removed afterwards, so
reading a pack also verifies it. A replay of `run.tar` writes
`run.replay.jsonl` beside the tar. `resume` appends to a trace and so needs
it unpacked.

### Diff

`tether diff <a> <b>` compares what two runs did, not when or how fast.
Within each step it lines up, in order:

| Event | Compared | Not compared |
| --- | --- | --- |
| `model.request` | message hashes, model, tools, params | provider |
| `tool.call` | name, arguments | id, server |
| `policy.decision` | outcome; for an `ask`, how it was answered | rule name, reason, budget |

and then how the runs ended: the last `run.state` and the outcome in
`run.finished`. It prints one line per step, the first divergence with both
sides, and for a request whose messages differ, the two messages read from
each trace's blobs. Exit status is 0 when the traces agree, 1 when they
diverge and 2 when one cannot be read or fails verification.

Left out on purpose: `ts`, `latency_ms`, `duration_ms`, budgets, hashes and
provider-assigned ids, which differ between any two live runs. A model's
reply is not compared directly either. It shows up in the next request's
message hashes, or in how the run ended if it was the last word. `provider`
is left out so that a replay, whose adapter is not the original one, still
compares equal. A different rule reaching the same outcome is not a
divergence; a changed policy is noted in the header and matters only where a
decision changes.

Alignment within a step is by position, so one extra tool call shifts
everything after it in that step. The first divergence is the one to read;
after it the runs are doing different things.

### Long traces

There is no index over a trace. The plan allowed for a SQLite one for
`inspect` and `diff` on traces over 100k events, to be measured before it was
built. Measured at v0.3, on an Apple M1 Max with a release build, on traces
made by `gen_trace` (an example in `tether-trace`) at about 400 bytes an
event:

| Events | File | `inspect` | `verify` | `diff` | `diff` memory |
| --- | --- | --- | --- | --- | --- |
| 100,000 | 41 MB | 0.5 s | 0.2 s | 0.5 s | 190 MB |
| 1,000,000 | 409 MB | 5.1 s | 2.1 s | 5.0 s | 1.8 GB |

Every command reads and checks the whole file at about 200,000 events a
second. `inspect` and `verify` hold one event at a time and stay under
10 MB. `inspect` piped into `head` returns at once.

So the index was not built. What it would speed up is going straight to one
step or one event, and no command does that yet. The cost that does grow is
`diff`'s memory: it holds both traces whole, about 900 bytes an event. That
is fine at 100k events and heavy at a million, and an index would not change
it. The fix, when a trace that long turns up, is for `diff` to keep only the
fields it compares as it reads.

### Eval

`tether eval <suite>` replays a suite of recorded tasks and checks each new
trace against assertions. It is replay, diff and a table: there is no eval
state and nothing in `tether-core` knows about it.

A suite is a directory:

```
suite/
  eval.toml
  write-hello.jsonl        a recorded task, named by its file
  write-hello.blobs/
  count-lines.tar          or a packed one
```

```toml
# eval.toml
[defaults]                 # for every task
no_deny = true
max_steps = 8

[task.write-hello]         # for one task; a field here replaces the default
outcome = "hello.txt"
max_usd = 0.05
tool_called = ["write_file"]
tool_not_called = ["exec"]
file_contains = [{ path = "hello.txt", text = "hello" }]
```

Each task is replayed as `tether replay` would, into `<suite>/.eval/` or
`--out`. With `--system` or `--model` the replay runs with
`--live-after-divergence`, so the task is asked again under the change from
the first request that differs, with real tools in a fresh workspace. Those
calls are billed. With neither, nothing goes live and no credentials are
needed: the suite checks its own recordings. `--policy` holds every task to
another policy. An ask made live goes unanswered, since nobody is attending
twenty tasks, so it halts that task.

`--model` asks a different model through the recorded provider. A request is
matched to the recording by its messages alone, so the changed model is
declared a divergence at the first request; `tether replay` takes the same
flag.

Assertions read the new trace and nothing else: no workspace is inspected and
no model judges the answer.

| Assertion | Passes when |
| --- | --- |
| (always) | the run's last state is `finished` |
| `outcome` | the outcome in `run.finished` contains the text |
| `max_steps` | the highest step is at most this |
| `max_usd` | `usd_used` in the last `budget.tick` is at most this |
| `no_deny` | no `policy.decision` is a deny and no `policy.answer` is a no |
| `tool_called` | each named tool has a `tool.call`; `write_file` or `sandbox.write_file` |
| `tool_not_called` | none of the named tools has one |
| `file_contains` | the last successful `write_file` call to the path has the text in its content |

`file_contains` sees only what `write_file` wrote. A file made or changed by
a command is not in the trace as content, so it cannot be asserted on. The
cost of a task is the whole run's: replayed calls keep their recorded price
and live calls are priced as they happen.

The output is one row per task (pass or fail, steps, cost, and where the new
trace first diverges from the recording), then each failed assertion in
words, then totals. The exit code is 0 when every task passes, 1 when any
fails and 2 when the suite cannot be run. A task that cannot be replayed
fails; the others still run. `--scripts <dir>` gives each task a scripted
stand-in for the live model, for tests and demos.

### OpenTelemetry export

`tether export otlp <run>` turns a trace into OpenTelemetry spans and sends
them to a collector. It reads the trace file and nothing else, so it works
on any trace after the fact, and `--follow` exports a live run by reading
the file as it grows (`TraceFollower` in `tether-trace`, which verifies the
chain as it goes and waits out a half-written last line). The run loop has
no exporter in it and no second code path.

One OpenTelemetry trace per run:

| Span | From | Notes |
| --- | --- | --- |
| `tether.run` | first event to last | Root. `run.state` changes are span events; totals from the last `budget.tick`; the outcome; an error status with the reason when the run halted |
| `step N` | first to last event of the step | Child of the run. A `checkpoint` is a span event. A step done again after a resume is a second span |
| `chat <model>` | `model.request` to `model.response` | Provider, model, token counts, stop reason, and the call's price from the tick that follows |
| `execute_tool <name>` | `tool.call` to `tool.result` | Tool, server, call id, duration; an error status when the result is an error |
| `policy.decision <outcome>` | the decision; for an ask, until its `policy.answer` | Child of the call it is about. Rule, reason, who answered. An ask's span is the time spent waiting |

Model and tool spans use the OpenTelemetry GenAI attribute names where one
exists (`gen_ai.operation.name`, `gen_ai.provider.name`,
`gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.tool.name` and
so on); everything else is under `tether.`. No message or tool content is
exported, only what happened and how long it took: prompts, arguments and
results stay in the trace.

Trace and span ids are derived from the run id and each event's `seq`, so
exporting a trace twice gives the same ids. A call or an ask with no answer
in the trace, because the run halted or is still going, is closed at the
last event and marked `tether.incomplete`.

With `--follow`, a span is sent when it completes, so a step arrives after
the calls inside it and the run's own span arrives last, when the run
finishes or halts, or on Ctrl-C. A halted run that is resumed later is a new
export.

Spans go out as OTLP over HTTP in the JSON encoding, `POST <endpoint>/v1/traces`,
which collectors accept on port 4318. This is written by hand in the CLI:
the format is small and stable, and the OpenTelemetry SDK crates would be
the largest dependency in the tree for one command. The endpoint is
`--endpoint`, else `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (the whole address),
else `OTEL_EXPORTER_OTLP_ENDPOINT`, else `http://localhost:4318`.
`OTEL_EXPORTER_OTLP_HEADERS` carries credentials for a hosted backend and
`OTEL_SERVICE_NAME` sets the service name, `tether` by default. `--out
<file>` writes the requests to a file, one per line, and sends nothing.
There is no gRPC transport and no retry: a collector that refuses is an
error.

### Trace viewer

`tether ui <run> [<run-b>]` shows a trace in the browser. The page is one
HTML file, `ui/index.html`, with plain JavaScript and no build step, built
into the binary with `include_str!`. A small server in the CLI serves it on
the loopback address with JSON endpoints beside it, and every endpoint is a
thin layer over `tether-trace`: the viewer shows nothing the CLI cannot
already print.

| Endpoint | What it returns | Same as |
| --- | --- | --- |
| `/api/runs` | For each run: its state, outcome, totals, and its steps with what stands out in each | `tether inspect`, summed |
| `/api/run/<a\|b>/step/<n>` | The events of a step, each with its one-line summary and the event whole | `tether inspect` |
| `/api/run/<a\|b>/blob/<hash>` | A stored message or tool result | the blob store |
| `/api/diff` | Per-step counts and every divergence | `tether diff` |

The page has the steps down the left as one line with a knot per step, the
events of the chosen step in the middle, and the chosen event in full on the
right, where a blob hash opens the content behind it. A denied call, an ask,
a failed tool and a halt are marked on the event and on its step, and the
line is cut at the step where a run halted. The page opens on the step that
most needs looking at: the halt, else the first denial, else the last step.
With two runs it opens on the comparison, at the step where they part, with
the two runs' events side by side.

The traces are read once, when the command starts; a run still being written
is shown as it was then. A trace that does not verify is still shown, with
the reason at the top.

A trace holds whatever the agent saw and said, so the server keeps it on the
machine:

- It listens on `127.0.0.1` only, on a free port unless `--port` names one.
- It answers only requests whose `Host` is that address or `localhost`, so
  another site's name pointed at the loopback address gets nothing.
- It sends no cross-origin header, so another site's page cannot read a
  response.
- The page's content security policy allows nothing but its own script and
  style, by a nonce made at each start, and no outside connection. Trace
  content is put on the page as text, never as markup.
- A blob is looked up by hash and by nothing else, so the endpoint cannot be
  used to read another file.

### Timings under replay

Four things in a trace come from live clock reads, so a replay that measured
them afresh would get different values every time:

| Field | Where | Computed from |
| --- | --- | --- |
| `ts` | every event | one clock read when the event is emitted |
| `latency_ms` | `model.response` | clock read before and after the model call |
| `duration_ms` | `tool.result` | clock read before and after the tool call |
| `wall_secs_used` | `policy.decision`, `budget.tick`, `run.finished` | clock read minus the run's start |

None of these reads is recorded as a `nondeterminism` event; only the values
derived from them are in the trace. A replay therefore restores them from the
recorded events rather than measuring them, which is design rule 4 applied to
time. As the run loop writes each event it finds the event's counterpart in
the recording, the next recorded event of the same kind within the same
call, and copies its `ts` and timing fields. The budget check before a call
uses the elapsed time on the next recorded budget, so a run that halted on
`max_wall_secs` halts at the same point. Under a changed policy a call can
have more or fewer events than it had; the ones it still has line up, and an
event with no counterpart takes the time of the one before it.

Telling `diff` to skip the fields instead would hide the symptom and leave
the wall-clock halt wrong. A replay `Clock` handing back recorded reads in
order would work only while the loop makes exactly the reads it made when the
trace was written, which a later change to the loop would break silently.

After a divergence the clock is live again. Elapsed time carries on from the
recorded figure, and timestamps jump from the recording's to now.

## SDK boundary

One gRPC service (`proto/tether.proto`) on a Unix socket; Python and
TypeScript SDKs are thin clients. The agent replaces two call sites: its
provider SDK call becomes `Complete`, its tool dispatch becomes `CallTool`.
`tetherd` starts on demand and exits after its last run.

| RPC | What it does |
| --- | --- |
| `CreateRun` | Starts a run: policy file, provider, sandbox, where the trace goes. Returns the run id, trace path and tool list |
| `Complete` | One model turn. Messages are a JSON list in Tether's form; the reply comes back as JSON to append unchanged |
| `CallTool` | One tool call, through policy and the sandbox. A denied call comes back as a result with `is_error` |
| `Finish` | The agent is done: records `run.finished` |
| `Events` | Streams the trace: every event so far, then each new one until the run ends |
| `Answer` | Resolves a `policy.ask`. The `Complete` or `CallTool` that asked is waiting on it |
| `Halt` | Trips the run's kill switch; with no call in flight the run halts at once |

An SDK holds no logic beyond mapping types. Messages, params, tool arguments
and results cross as JSON strings, so the proto does not have to follow every
provider's message shape. Errors are gRPC statuses: `NOT_FOUND` for an
unknown run, `INVALID_ARGUMENT` for bad JSON or an unknown tool,
`FAILED_PRECONDITION` for a run that is halted or finished, with the reason
in the message.

The daemon (`tetherd --socket <path> [--idle-secs N]`):

- **Socket.** `$TETHER_SOCKET`, else `.tether/tetherd.sock`, readable and
  writable by its owner only, since whoever can reach it can run tools as
  that user. A leftover socket file nobody answers is replaced; if a daemon
  already answers there, a second one exits and leaves it to the first.
- **Lifetime.** It exits, and removes its socket, once it has had no run in
  progress and no event stream open for the idle period (10 s by default).
  That covers both a daemon started and never used and one whose last run is
  over.
- **Runs.** One call at a time per run. `Answer` and `Halt` do not wait
  behind a call in flight, which is what lets them reach a run that is
  suspended on an ask.
- **Build.** The service is generated from the proto at build time with a
  `protoc` binary shipped as a crate, so `cargo build` needs no system
  install.

`tether run [--policy <file>] [--dir <dir>] -- <command>` is the on-demand
start from the CLI: it starts the daemon if nothing answers on
`<dir>/tetherd.sock`, runs the command as the agent with `TETHER_SOCKET`,
`TETHER_DIR` and `TETHER_POLICY` set, and exits with the command's status.
The daemon it starts is the `tether` binary itself (a hidden `daemon`
subcommand calling the same `tetherd::serve`), in its own process group so
Ctrl-C meant for the agent does not take it down, logging to
`<dir>/tetherd.log`.

The Python SDK (`sdks/python`, package `tether-agents`, module `tether`) is
the first client. `tether.Run(...)` connects to the socket, starting the
daemon if nothing answers, and creates a run; `run.complete` and
`run.call_tool` are one request each; `run.tools`, `finish`, `halt`,
`answer` and `events` map to the rest. As a context manager, a run that
leaves its block without a word is finished, or halted if the block raised.
The SDK holds no logic: messages are plain dicts in Tether's form, passed
through as JSON, and the only helpers build a user or system message and the
message of tool results. Its gRPC stubs are generated from the proto and
committed, so installing it needs no protoc. `examples/raw-loop-py` is the
Python twin of `examples/raw-loop`: the two agents leave traces that
`tether diff` finds no divergence between.

The TypeScript SDK (`sdks/typescript`, package `tether-agents` on npm) is the
same client with JavaScript's spelling: `Run.start(...)`, `run.complete`,
`run.callTool`, `run.tools`, `finish`, `halt`, `answer`, and `events` as an
async iterator. Where Python has a context manager it has
`withRun(options, fn)`, which finishes the run when `fn` returns and halts it
when `fn` throws. It uses `@grpc/grpc-js` and loads the service definition at
start from a copy of `proto/tether.proto` shipped in the package, so it has
no generated code. `examples/raw-loop-ts` is the third twin, and its trace
diffs clean against the other two.

One thing every client over a Unix socket must do: name an HTTP/2 authority.
gRPC clients default to sending the socket's path, which the daemon's HTTP
stack rejects. The SDK sets `localhost`.

An agent that crashes or is killed never says so; it just stops calling. The
daemon halts a run that has had no call from its agent for
`idle_timeout_secs` (in `[budget]`, default 600, 0 for never), with the
reason `kill switch: no call from the agent for N s`. The halt is in the
trace, the run's MCP servers are stopped, the run can be resumed, and with
nothing left in progress the daemon goes on to exit when idle. A call in
flight is not idleness, however long the model or the tool takes: the clock
runs only between calls. An agent that works by itself for longer than the
limit between calls needs a larger one.

Limits today: the daemon has only the `process` sandbox, and an ask from a
run with no rules waits its full timeout unless something calls `Answer`.

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
`inspect`, `diff` and `verify` without a daemon. `tether-cli` also links
`tetherd` as a library, for the one function that serves the daemon, so that
`tether run -- <command>` can start it without a second binary to find.

## Decisions

| Question | Decision | Escape hatch |
| --- | --- | --- |
| Streaming | One collapsed `model.response` with `stream: true`, `ttft_ms` | None built. Writing the raw deltas to a blob is the way, if they are ever wanted |
| Blob storage | Hybrid: inline under 32 KB, content-addressed sidecar per message | `tether pack` for one-file portability |
| Multi-agent | One trace per run, `agent` field, single writer task | Separate process gets its own run id plus `run.child` |
| Unattended `ask` | 600 s timeout, then Halted (resumable) | `on_timeout = "deny"` per rule |
| MCP client | `rmcp` behind the trait, cargo feature | The trait: another client can go behind it |
| Hash chain | Keep; BLAKE3 in the writer task | None: every trace is chained |
