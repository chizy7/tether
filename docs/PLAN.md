# Tether: Project Plan

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
| v0.5 it connects | after launch | MCP client for external servers over stdio and HTTP, one tool list, rules by server | A real model, the sandbox and an MCP server under one policy |
| v1.0 it isolates | after launch | Firecracker backend, stable trace schema, plugin API for custom policies | Untrusted agent runs in a microVM with the same trace |

The week numbers are the plan's sizes, not dates: the phases shipped faster
than that. Reassess after launch on stars, issues, and whether anyone besides
the maintainer has wrapped an agent with it.

## Shipped

What each release contains is in [CHANGELOG.md](../CHANGELOG.md). The
checklists the phases were built from are in this file's git history.

| Phase | Released | Demo |
| --- | --- | --- |
| v0.1 it records | 2026-11-18, untagged | [raw-loop](../examples/raw-loop) |
| v0.2 it refuses | 2026-11-18, untagged | [refuses](../examples/refuses) |
| v0.3 it replays | 2026-11-21, v0.3.0 | [replays](../examples/replays) |
| v0.4 it evals | 2026-11-22, v0.4.0 | [evals](../examples/evals) |
| v0.5 it connects | 2026-11-22, v0.5.0 | [mcp-filesystem](../examples/mcp-filesystem) |

0.4.1 was a correction and not a phase: the docs and package keywords had
said Tether connected to MCP servers before any client existed. v0.5 built
the client.

## Next

No phase is planned yet. Work is issues, and what is wanted next is on the
[roadmap](https://github.com/users/chizy7/projects/8): the v1.0 items
(Firecracker, a policy plugin API, a stable trace schema) and what the 0.5.0
review left open.

Still to do from the launch, by the maintainer: the `mcp` topic back on the
repository, and the post to the MCP community, which waited for the client.

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
| Models | Anthropic, and any server with an OpenAI-style chat completions endpoint, which covers local ones such as Ollama | One trait, swap behind a flag |
| Observability | OpenTelemetry export | Plugs into what teams already run |
| Release | cargo-dist, `curl \| sh`, `pip install tether-agents` | One-line install by v0.3 |

## Risks

| Risk | Response |
| --- | --- |
| Scope creep into framework territory | Every PR answers: does this run agents or build them? |
| Sandboxing is the hard part; overclaiming destroys trust | Ship `process` as the labelled baseline; add Docker and Firecracker in order |
| Solo maintainer burnout | Phases are small and each ends in a release; stop at any of them |
| A funded player ships the same thing | Defend with the trace format and the replay story |
| MCP protocol shifts | One trait, `rmcp` behind it, and a list of protocol versions each release will talk to |
| Thin demos | Each phase is its demo; cut features before the demo |
