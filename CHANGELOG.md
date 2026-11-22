# Changelog

What changed in each release of Tether, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions before 1.0
may break the command line and the SDKs between minor versions; the trace
format on disk is frozen at v0 from 0.4.0 and is covered by a compatibility
test.

## Unreleased

Nothing yet.

## 0.5.0, 2026-11-22

"It connects." Tether talks to external MCP servers.

### Added

- `[[mcp_server]]` tables in `tether.toml`: an MCP server over stdio (`command`,
  `args`, an `env` allowlist for the child beside `PATH`) or streamable HTTP
  (`url`, `headers`), with a `timeout_secs` for the handshake and for each
  call. Validated when the policy loads. A header value takes `${NAME}` from
  the environment, so a token is never written in the policy file, which
  every trace records.
- `tether-mcp::client::ExternalServer`: the MCP wire-protocol client, built on
  `rmcp` 3.x behind the `McpClient` trait. `initialize` with a check of the
  negotiated protocol version, `tools/list`, `tools/call`, timeouts, and a
  child process that stops with the run. A server that never answers
  `initialize` fails the run at start.
- `tether-mcp::Registry`: merges the built-in sandbox server with every
  configured server into the one tool list the agent sees. A name collision
  is exposed as `<server>__<tool>`; the sandbox keeps the bare name. Rules
  see every tool as `<server>.<tool>`.
- `run.created.mcp_servers`: each server's id, transport, negotiated protocol
  version and reported name. Inside `data`, so the frozen envelope is
  unchanged.
- Policy rules can match on `server`, so one rule covers a whole server.
- `tether tools [--policy] [--json]`: the merged tool list by server without
  starting a run.
- `examples/mcp-filesystem`: the demo against the reference filesystem server.
- The `rmcp` cargo feature on `tether-mcp` and `tether-core`, on by default.
  Without it a policy naming a server fails at start with a clear error.
- `tether resume`, and a replay or eval that goes live after leaving the
  recording, connect the servers the policy names, so the external tools
  work there too. A replay that stays on the recording connects nothing.
- `--trust-recorded-servers` on `replay`, `eval` and `resume`. A trace's
  recorded policy could name any command, so those commands start a server
  only from a `--policy` file or with this flag, and print what it runs
  first.
- `tether inspect` shows a call to an external server as `<server>.<tool>`.
- `SECURITY.md` says what an external MCP server can do: it is not
  sandboxed, and policy is the only check on it.

### Security

Found in a review of the whole codebase before this release.

- Tool arguments sent as a JSON list were run by the built-in tools but had
  no `argv` or `path` for a rule to match, so they passed every such rule.
  The built-in tools now take a JSON object only.
- `list_dir` checked no path and listed any directory on the machine. It is
  held to the workspace like the other file tools.
- A `deny` or `ask` on a `path` or command name now ignores letter case, and
  matches a path that climbs out with `..`. Both used to slip past.
- A `tool` pattern such as `fs.*` now covers a tool whose name has a `/`.
- A blob is checked against its hash and must be a plain file on every
  read. A trace directory could carry a blob that was a link to a local
  file, and `tether replay` would copy that file into its output.
- A crafted trace could crash `replay` and `eval`, or hang them for minutes
  through the pattern matcher. Both are fixed; matching no longer backtracks.
- Requests carrying credentials (model providers, HTTP MCP servers, OTLP
  export) no longer follow redirects.
- `tether inspect`, `tether tools` and the ask prompt show control
  characters as escapes, so a trace or a server cannot rewrite the terminal.
- The ask prompt shows `path` and `argv` first and says how much it cut, so
  a long `content` cannot hide what is being approved.
- The daemon removes only a socket at its socket path, never another file.

### Fixed

- `exec` with large input could hang for ever, past the time limit: input,
  output and the limit now run together.
- A command's process group is killed when it returns, not only when it
  times out, so background jobs do not outlive it.
- Output is capped at 4 MiB per stream and `read_file` at 16 MiB, so a
  command that prints without end cannot exhaust memory or disk.
- A trace is locked by its writer. `tether resume` on a run that was still
  alive wrote a second run into the same file and broke it for good.
- The trace is synced to disk at each checkpoint and on a halt, not only at
  the end.
- The daemon could shut down as idle while a run was being created, and
  its halt did nothing for a run left waiting on an ask.
- A run under the daemon whose agent crashed, or was killed, stayed in
  progress for ever and kept the daemon and its MCP servers alive. The
  daemon now halts a run with no call from its agent for `idle_timeout_secs`
  (`[budget]`, default 600, 0 for never); the run can be resumed. (#45)
- A second Ctrl-C exited `tether` but left the sandbox command it was
  running. The command and what it started are now killed first.
- A socket path too long for the system gets an error that says so.
- A dollar budget on a model with no price now warns that it cannot apply.

### Changed

- Every tool of an external server is `external = true`; replay returns the
  recorded result and starts nothing. Replay also carries the recorded
  `mcp_servers`, so a replayed `run.created` equals the original, including
  for a trace written before 0.5, which has none.
- Logging defaults to `info,rmcp=warn` and goes to stderr; `RUST_LOG`
  overrides it.
- `mcp` is back in the PyPI and npm package keywords, this time truthfully.

## 0.4.1, 2026-11-22

Documentation and package metadata only. No code change.

### Fixed

- The README, `docs/ARCHITECTURE.md`, `CLAUDE.md`, and the `mcp` keyword on the
  PyPI and npm packages said Tether connects to MCP servers. It does not: the
  `McpClient` trait has one implementation, the built-in sandbox, in-process,
  and nothing speaks the MCP wire protocol. The docs now say what exists and
  mark the external-server client as planned. The work is tracked in #56 and
  is the whole of v0.5. (#57)

## 0.4.0, 2026-11-21

"It evals." The launch release.

### Added

- `tether eval <suite>`: replays every recorded task in a directory and checks
  each against the assertions in its `eval.toml` (outcome text, step and cost
  limits, no refusals, tools called or not called, file contents). With
  `--system` or `--model` each task is asked again under the change. Reports
  pass, fail and cost per task. (#31)
- `tether replay --model`: replay a recording against a different model. (#31)
- `tether export otlp`: a trace as OpenTelemetry spans over OTLP/HTTP JSON,
  once or following a live run. Messages, tool arguments and tool results
  are not exported. (#32)
- TypeScript SDK, published to npm as `tether-agents`, and the `raw-loop-ts`
  example. (#33)
- `tether ui`: a trace viewer in the browser, served on this machine only.
  Shows the steps of a run, where policy refused or asked, and with two traces
  where they diverge. (#34)
- `examples/evals`: a suite of 20 recorded tasks and a prompt that fails some
  of them. (#35)
- `examples/openai-agents-sdk`: the OpenAI Agents SDK loop with its model and
  tool calls routed through Tether. (#36)
- `CONTRIBUTING.md`, `SECURITY.md`, this file, and issue and PR templates.
  (#37)
- `docs/REPLAY.md`: how replay works and where it stops working. (#39)

### Changed

- The trace schema is frozen at v0. A test reads traces written by 0.3.0, and
  a change to the format on disk now needs a version bump and a reader for
  the old version. (#30)
- The README is rewritten for a first-time reader, and its images moved to
  `docs/media`. (#38, #53)

### Removed

- The placeholder for a Claude Agent SDK example. That SDK makes its model
  calls itself and has no hook on them, so a run could not be recorded in
  full or replayed. (#36)

## 0.3.0, 2026-11-21

"It replays." The first tagged release, and the first with binaries.

### Added

- `anthropic` model adapter with streaming, a price table, and the cost of
  each call recorded in `budget.tick`. (#15)
- `tether diff`: aligns two traces by step and reports where they diverge.
  (#16)
- Checkpoint events after each step, with the sandbox snapshot id. (#17)
- `tether replay`: runs a trace again, answered from the recording, with no
  network and no cost. Policy is evaluated again, so a changed policy can be
  tested against old runs. Timings are restored from the trace. (#18)
- `tether resume`: continues a halted or crashed run from its last
  checkpoint, appending to the same trace. (#19)
- `tether pack` and `tether unpack`: a trace and its blobs as one tar, which
  `inspect`, `verify`, `diff` and `replay` read in place. (#20)
- `tetherd`: a gRPC daemon on a Unix socket, started on demand, which is what
  the SDKs talk to. (#21)
- Python SDK, published to PyPI as `tether-agents`, and the `raw-loop-py`
  example. (#22, #26)
- `examples/replays`: records 20 tasks, changes the prompt, replays and
  compares. (#23)
- `openai_compat` model adapter for servers that speak OpenAI-style chat
  completions. (#24)
- Release binaries for macOS and Linux on arm64 and x86-64, and a one-line
  shell installer. (#25)

### Removed

- The unused `rusqlite` dependency. An index for long traces was measured and
  found unnecessary: `inspect` takes half a second on 100,000 events. (#27,
  #28)

## 0.2.0, 2026-11-18

"It refuses." Not tagged; built from source only.

### Added

- Policy rules in TOML: allow, deny or ask, matched on tool name and
  arguments. The name of the rule that decided is recorded in
  `policy.decision`, and the policy itself in `run.created`. (#5, #6)
- Budgets, checked before every model and tool call, with a `budget.tick`
  event after each. (#7)
- `ask`: `tether run` puts the question to the terminal. An unanswered ask
  halts the run after a timeout, or denies if the rule says so. (#8)
- Kill switch: Ctrl-C or a `<run>.halt` file stops a run at the next event
  boundary. (#9)
- `process` sandbox: CPU time and open file limits, and on Linux a memory
  cap and a seccomp filter. (#10)
- `docker` sandbox backend, behind the `docker` cargo feature. (#11)
- `examples/refuses`: an agent tries `rm -rf` and is blocked. (#12)

## 0.1.0, 2026-11-18

"It records." Not tagged; built from source only.

### Added

- The trace: one JSON event per line, a BLAKE3 hash chain over the events,
  and large payloads stored as content-addressed blobs.
- The run loop, with every model call and tool call recorded.
- `process` sandbox: commands run in a workspace with a cleared environment
  and a time limit.
- `fake` model adapter, which answers from a script, so tests need no API
  key.
- `tether run`, `tether inspect` and `tether verify`.
- `examples/raw-loop`: a small agent wrapped by Tether.
