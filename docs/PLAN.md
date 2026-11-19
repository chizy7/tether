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
- [ ] Write down for v0.3: `latency_ms` and `duration_ms` come from live clock reads, so replay must restore them from the recorded events or `diff` flags every event

## Stack decisions

| Layer | Choice | Why |
| --- | --- | --- |
| Core | Rust stable, tokio | Fits the safety story; sandbox code needs low-level control |
| CLI | clap | Standard |
| Serialization | serde, JSONL | Human-readable, `jq`-friendly, append-only |
| Hashing | BLAKE3 | Several GB/s per core; a million events chain in well under a second |
| Trace index | SQLite via rusqlite | Zero-ops local cache for `inspect` and `diff`; never the source of truth |
| SDK boundary | tonic (gRPC) over Unix socket | One protocol for Python and TypeScript; less maintenance than bindings |
| MCP client | `rmcp` behind a trait, cargo feature | Protocol tracking is someone else's job; the trait keeps the exit |
| Sandbox | `process` default, `docker` v0.2, `firecracker` v1.0 | Ship the honest baseline first; claim no more than it isolates |
| Models | Anthropic first, OpenAI-compatible v0.3, Ollama after | One trait, swap behind a flag |
| Observability | OpenTelemetry export | Plugs into what teams already run |
| Release | cargo-dist, `curl \| sh`, `pip install tether-sdk` | One-line install by v0.3 |

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
