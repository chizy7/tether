# Tether: Project Plan

Living copy: https://claude.ai/artifact/VDSQ64VhzWGAE9LSxXXean

**One-liner:** Run agents you don't trust on infrastructure you do. A
model-agnostic, MCP-native runtime that sandboxes, budgets, records and replays
any agent.

## Positioning

A framework is the brain wiring: loop structure, tool descriptions, memory.
A harness is everything that makes an agent safe to run unattended: isolation,
limits, an audit trail, replay of a failure and a test of the fix against it.
LangGraph, OpenAI Agents SDK, Pydantic AI, Mastra and the Claude Agent SDK
compete on the first. Almost nobody ships the second well.

The wedge is the trace. Every model call, tool call and policy decision is an
event in one open format. Replay, diff and eval fall out of that one decision.

## Non-goals

Tether wraps your agent; it does not replace it. PR test: does this run
agents, or build them?

- No prompt templates or prompt management
- No RAG, vector stores or retrieval
- No agent-builder UI or visual workflow editor
- No opinion on how the agent loop is structured
- No hosted service in v1; local and self-hosted only

## Phases

Ship on the 4-week cadence even when a phase is thin. A harness nobody can try
is a design doc. Each phase is named by what it proves; the demo is the
acceptance test. Cut features before cutting the demo.

| Phase | Weeks | Ships | Demo |
| --- | --- | --- | --- |
| v0.1 it records | 1 to 4 | Core loop, Anthropic adapter, process sandbox, JSONL traces, `tether run`, `tether inspect`, `tether verify` | Wrap a toy coding agent and show a full trace |
| v0.2 it refuses | 5 to 8 | Policy engine, budgets, `ask` mode with CLI prompt, kill switch, Docker sandbox | Agent tries `rm -rf`, Tether blocks it, trace shows why |
| v0.3 it replays | 9 to 12 | Checkpoint and resume, deterministic replay, `tether diff`, `tether pack`, OpenAI adapter, `tetherd` and Python SDK, one-line install | Fix a prompt, replay 20 recorded tasks, diff outcomes |
| v0.4 it evals | 13 to 16 | `tether eval` over a trace suite, OpenTelemetry export, TypeScript SDK, trace viewer. **Launch.** | Eval suite and viewer on the README |
| v1.0 it isolates | after launch | Firecracker backend, stable trace schema, plugin API for custom policies | Untrusted agent runs in a microVM with the same trace |

Launch gate at v0.4: reassess on stars, issues, and whether anyone besides the
maintainer has wrapped an agent with it.

## Week one checklist

By the end of week two, one real agent produces one real trace that
`tether inspect` can read. Everything else waits.

- [x] Repo under Apache 2.0, CI for fmt, clippy, tests, MSRV
- [x] Workspace skeleton: nine crates, traits in place, `cargo check` clean
- [x] Trace schema v0 written as a spec (`TRACE_FORMAT.md`) before the writer
- [x] `tether-trace` writer with BLAKE3 chain and blob spill; reader with verification
- [x] `fake` model adapter so tests need no API key
- [x] Core run loop driving one step: model request, tool call, policy decision, trace events
- [x] One end-to-end: wrap a 50-line agent, produce a trace, `tether inspect` it
- [x] README demo GIF placeholder replaced with the real thing

v0.1 shipped 2026-11-18: 19-event trace from the raw-loop example, `verify` passes, 36 tests.

## v0.2 checklist: it refuses

Demo that closes the phase: the agent asks to run `rm -rf`, policy blocks it,
`tether inspect` shows the `policy.decision` with the rule that matched. Work
in this order; each item is one PR.

- [x] TOML rule engine in `tether-policy`: `[[rule]]` tables, `match` on tool name, `argv0` and `path` globs, first match wins, default `ask`; the matched rule's name goes into `policy.decision.rule`
- [x] Policy snapshot (full TOML) recorded in `run.created.policy`, replacing the `null`
- [x] Budget enforcement: check tokens, usd, wall clock and tool calls before every model and tool call; emit `budget.tick` after each; a tripped budget halts with `run.state` reason naming the budget
- [x] `ask` answered from the CLI prompt in `tether run`; `ask_timeout_secs` (default 600) moves the run to Halted; `on_timeout = "deny"` per rule
- [x] Kill switch: `<run>.halt` file watch plus SIGINT, both halt at the next event boundary, never mid-write
- [x] `process` backend: rlimits (cpu, memory, open files) and the Linux seccomp filter; then make the README's sandbox paragraph true
- [x] `docker` backend behind the `docker` feature: workspace mount, network namespace per `SandboxSpec.network`, snapshot via `commit`
- [x] Demo: `examples/refuses` with a scripted `fake` run that attempts `rm -rf`; GIF of the blocked call in `inspect`
- [x] Write down for v0.3: `latency_ms` and `duration_ms` come from live clock reads, so replay must restore them from the recorded events or `diff` flags every event. Written up in `ARCHITECTURE.md`, "Timings under replay", which also covers `ts` and `wall_secs_used`

v0.2 shipped 2026-11-18: nine PRs, 86 tests, `refuses` demo GIF.

## v0.3 checklist: it replays

Demo that closes the phase: record 20 small tasks against a real model, change
the system prompt, `tether replay --live-after-divergence` all 20, `tether diff`
each pair, and read the outcomes off one table.

A gap carried from v0.1: the Anthropic adapter never landed. Every trace so far
came from the `fake` adapter, so Tether has not yet recorded a real model call.
That is item one, because replay of fake traces proves nothing. Items are in
dependency order; the last three can slip to v0.4 without hurting the demo.

- [x] `anthropic` adapter: Messages API, tool use, streaming collapsed into one `model.response` with `ttft_ms` and `stream: true`, raw body as a blob, `pricing.toml` with the price recorded in `budget.tick`. Key from `ANTHROPIC_API_KEY`. Tests stay on `fake`; one ignored integration test runs when the key is set. First real trace from `examples/raw-loop --model anthropic`
- [x] `tether-trace::diff` and `tether diff <a> <b>`: align by `step`, compare `model.request` message hashes, `tool.call` name and arguments, and `policy.decision` outcome; report the first divergence and a per-step summary. Ignore `ts`; the timing fields are restored on replay, not skipped here
- [x] `checkpoint` events every N steps (default 1) with the sandbox `snapshot` id when the backend supports it (`docker` yes, `process` returns `Unsupported` and the field is null)
- [x] Replay runtime: `replay` model adapter matching `model.request` by message hashes, replay sandbox returning recorded `tool.result`, and timings restored from the recorded events exactly as "Timings under replay" says. `tether replay <run>` writes a new trace; `tether diff` of the two is empty by construction, and that is the acceptance test. `--live-after-divergence` switches to the live adapter at the first mismatch
- [x] `tether resume <run>`: read to the last `checkpoint`, restore the snapshot or replay `write_file` and `exec` into a fresh sandbox, rebuild message history from blob hashes, `run.state = running` with reason `resumed`, continue appending to the same trace
- [x] `tether pack <run>` and `tether unpack`: one tar of JSONL plus blobs, verified on pack and on unpack; `inspect`, `verify`, `diff` and `replay` accept the tar directly
- [x] `tetherd`: tonic service from `proto/tether.proto` on a Unix socket, started on demand by the CLI and SDK, exits after its last run. Use a vendored `protoc` (`protoc-bin-vendored` or prost's) so `cargo build` never needs a system install
- [x] Python SDK in `sdks/python`: `Run`, `complete`, `call_tool`, `tools`, context manager; thin gRPC client, no logic. `examples/raw-loop-py` is the Python twin of `raw-loop`
- [x] Demo: `examples/replays` records 20 tasks, edits the prompt, replays with `--live-after-divergence`, diffs, prints the table; GIF
- [x] (may slip) `openai_compat` adapter behind its feature: Chat Completions shape, streaming collapsed, `OPENAI_BASE_URL` and `OPENAI_API_KEY`, prices from a `TETHER_PRICING` table
- [x] (may slip) cargo-dist release workflow, `curl | sh` install, `pip install tether-agents` on PyPI: one `vX.Y.Z` tag runs both, see `docs/RELEASING.md`
- [x] (may slip) `rusqlite` index for `inspect` and `diff` on traces over 100k events; measure before building. Measured and not built: both take half a second at 100k events, see "Long traces" in `docs/ARCHITECTURE.md`

v0.3 shipped 2026-11-21 as release v0.3.0: fourteen PRs, 164 tests, binaries for
every platform via cargo-dist, `tether-agents` on PyPI, `replays` demo GIF.

## v0.4 checklist: it evals, and it launches

Demo that closes the phase: `tether eval` runs a suite of recorded tasks
against a new prompt or model and prints pass, fail and cost per task, then
`tether ui` opens the one that failed and shows the step where it went wrong.
The launch post links that GIF.

Two halves. Items 1 to 6 are product and ship in order. Items 7 to 13 are the
launch and can run in parallel with them from week 14. The phase is done when
the launch post is live, not when the code is merged.

Product:

- [x] Freeze the trace schema: `TRACE_FORMAT.md` status becomes "v0, frozen 2026-11-21"; from here `v` bumps only for a breaking change and a reader for the old `v` stays in `tether-trace`. Add a `compat` test that reads the v0.3.0 demo traces checked into `crates/tether-trace/tests/fixtures/`
- [x] `tether eval <suite>`: a suite is a directory of recorded traces plus `eval.toml`. Assertions are over the trace only: `outcome` matches, `max_steps`, `max_usd`, `no_deny`, `tool_called` / `tool_not_called`, `file_contains` on a sandbox path. Each task replays with `--live-after-divergence` under the new `--system` or `--model`, then the assertions run. Output: one row per task (pass / fail, steps, usd, first divergence), totals, exit code 1 on any failure. No model-as-judge; that is agent-building
- [x] `tether export otlp <run> [--follow]`: one OpenTelemetry trace per run, one span per step, child spans for `model.*`, `tool.*` and `policy.decision`, attributes from the event. Reads the trace file, so it works live by tailing and after the fact on any trace; no second code path inside `tether-core`
- [x] TypeScript SDK in `sdks/typescript`: same surface as Python (`Run`, `complete`, `callTool`, `tools`), `@grpc/grpc-js` over the Unix socket, no logic. `examples/raw-loop-ts`. Publish as `tether-agents` on npm to match PyPI
- [ ] `tether ui <run> [<run-b>]`: one HTML file embedded in the binary with `include_str!`, served on localhost with a JSON endpoint over `tether-trace`. Steps down the left, events in the middle, blob content on click, policy decisions and halts highlighted; with two runs, the diff view. Vanilla JS, no build step. Built last of the product items, after the CLI can do everything it shows
- [ ] Demo: `examples/evals` with a 20-task suite, a deliberately regressed prompt that fails three, `tether eval` output, `tether ui` on a failure; GIF

Launch:

- [ ] Framework examples: `examples/claude-agent-sdk` and `examples/openai-agents-sdk` are README placeholders today and the README promises them. Each wraps the framework's own loop through the Python SDK in under 60 lines, or the README promise comes out
- [ ] Repo hygiene: `CONTRIBUTING.md` (the PR question, design rules, how a phase works), `SECURITY.md` (what the sandbox does and does not isolate, how to report), `CHANGELOG.md` back-filled from v0.1.0, issue and PR templates, `good-first-issue` on five real items
- [ ] README rewrite for a stranger: lead with the `evals` GIF, then the two-call-site code block, then install, then the honest sandbox note. Three GIFs is two too many above the fold; move `refuses` and `replays` into a "More" section
- [ ] `docs/REPLAY.md`: the long write-up. Why the trace is the only state, per-message blobs, timings under replay, what breaks determinism and what Tether does about it. This is the launch essay; it goes on HN as the link, with the repo in the first line
- [ ] Name collision check: "Tether" is also the USDT stablecoin, which owns the search results. Decide now whether to keep the name (and lean on `tether-agents` everywhere a package name appears) or rename before launch, when it is still cheap. Record the decision in the PLAN either way
- [ ] Public roadmap: GitHub Project with v1.0 items (Firecracker, policy plugin API, schema v1 if ever) so the first "what's next?" issue has an answer
- [ ] Launch: tag v0.4.0, HN post linking `docs/REPLAY.md`, r/rust, the MCP community Discord, one thread. Then two weeks of answering issues before touching v1.0. Reassess at the end of those two weeks: stars, issues from strangers, and whether anyone besides the maintainer has wrapped an agent

## Stack decisions

| Layer | Choice | Why |
| --- | --- | --- |
| Core | Rust stable, tokio | Fits the safety story; sandbox code needs low-level control |
| CLI | clap | Standard |
| Serialization | serde, JSONL | Human-readable, `jq`-friendly, append-only |
| Hashing | BLAKE3 | Several GB/s per core; a million events chain in well under a second |
| Trace index | SQLite via rusqlite, if one is ever needed | Zero-ops local cache for `inspect` and `diff`; never the source of truth. Not built: measured in v0.3 and not needed yet |
| SDK boundary | tonic (gRPC) over Unix socket | One protocol for Python and TypeScript; less maintenance than bindings |
| MCP client | `rmcp` behind a trait, cargo feature | Protocol tracking is someone else's job; the trait keeps the exit |
| Sandbox | `process` default, `docker` v0.2, `firecracker` v1.0 | Ship the honest baseline first; claim no more than it isolates |
| Models | Anthropic first, OpenAI-compatible v0.3, Ollama after | One trait, swap behind a flag |
| Observability | OpenTelemetry export | Plugs into what teams already run |
| Release | cargo-dist, `curl \| sh`, `pip install tether-agents` | One-line install by v0.3 |

## OSS hygiene

- README opens with a GIF: agent misbehaves, Tether blocks it, trace shows the decision
- One-line install by v0.3
- `examples/` with wrappers for Claude Agent SDK, OpenAI Agents SDK and a raw loop
- Public roadmap in GitHub Projects, `good-first-issue` on real work
- `TRACE_FORMAT.md` frozen from v0.3; the format is what other tools can adopt
- Launch at v0.4: Hacker News, r/rust, MCP community, one long write-up on replay

## Risks

| Risk | Response |
| --- | --- |
| Scope creep into framework territory | Every PR answers: does this run agents or build them? |
| Sandboxing is the hard part; overclaiming destroys trust | Ship `process` as the labelled baseline; add Docker and Firecracker in order |
| Solo maintainer burnout | Commit to 16 weeks; reassess at v0.4 |
| A funded player ships the same thing | Defend with the trace format and the replay story |
| MCP protocol shifts | One trait, one pinned protocol version per release |
| Thin demos | Each phase is its demo; cut features before the demo |
