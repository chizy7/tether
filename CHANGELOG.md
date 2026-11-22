# Changelog

What changed in each release of Tether, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions before 1.0
may break the command line and the SDKs between minor versions; the trace
format on disk is frozen at v0 from 0.4.0 and is covered by a compatibility
test.

## Unreleased

Nothing yet.

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
  once or following a live run. Message content is not exported. (#32)
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
