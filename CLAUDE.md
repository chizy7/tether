# Tether

Run agents you don't trust on infrastructure you do.

Tether is a Rust runtime that wraps any agent. Every model call, tool call and
policy decision becomes an event in one append-only trace; replay, diff, eval
and checkpoint are all built on that trace. Tether is a **harness**, not a
framework: it runs agents, it does not build them.

Read `docs/ARCHITECTURE.md` before touching `tether-core`, `tether-trace` or
`tether-types`. Read `docs/PLAN.md` to see which phase a feature belongs to.

## The one PR question

**Does this run agents, or build them?** If it builds them, it does not belong
here. Prompt templates, RAG, memory, agent-builder UIs and opinions about loop
structure are out of scope, permanently.

## Design rules (every crate, every PR)

1. **The trace is the only state.** A run is reconstructable from its JSONL.
   No crate keeps state that is not also an event. Checkpoints index the trace.
2. **Every side effect passes through policy.** Model calls, tool calls, file
   and network access go through `tether-policy` first; the decision is
   recorded whether it was allow, deny or ask.
3. **Every tool is an MCP tool, through one trait and one policy path.** The
   built-in sandbox implements `McpClient` in-process as the server named
   `sandbox`; external servers are connected over stdio or HTTP by `rmcp`
   behind the same trait and merged by the `Registry`. Rules see
   `<server>.<tool>` and can match a whole server.
4. **Nondeterminism is captured at the boundary.** Model responses, tool
   outputs, clock reads and random seeds are recorded on the way in so replay
   substitutes them. Core code reads time and randomness through `Clock` and
   `Rng` traits, never `std::time` or `rand` directly.
5. **The core has no opinion about the agent.** No prompt handling, no loop
   structure. The agent emits model and tool requests; Tether answers them.

A PR that breaks one of these needs a written reason in its description.

## Settled decisions (do not reopen without a new ADR in docs/)

- Rust stable, tokio, clap, serde, tonic for the SDK boundary. No trace index: measured in v0.3 and not needed. If one is ever built it is SQLite via rusqlite, a cache and never the source of truth.
- Trace: JSONL, one event per line, `blake3:` hash chain computed by the single writer task.
- Blobs: payloads over 32 KB go to `<run>.blobs/blake3/ab/abcd...`, content-addressed **per message**. A `model.request` stores its history as a list of blob hashes.
- Streaming: collapsed into one `model.response` with `stream: true` and `ttft_ms`.
- Multi-agent: one trace per run, `agent` field on every event, one writer.
- Unattended `ask`: 600 s timeout, then `Halted` (resumable). `on_timeout = "deny"` is per-rule opt-in.
- MCP client: `rmcp` 3.x behind the `McpClient` trait, `rmcp` cargo feature on by default. Checked against the real crate in v0.5. No hand-rolled fallback; if `rmcp` ever blocks, the trait is the seam.
- Sandbox: `process` default and explicitly not a security boundary; `docker` v0.2; `firecracker` v1.0.
- Name: "Tether". Packages are `tether-agents`; where a name is searched for, write "Tether, a harness for agents", not the bare word.

## Layout

```
crates/tether-types     shared types, serde only, no logic
crates/tether-trace     writer, reader, hash chain, blob store, diff
crates/tether-policy    rules, budgets, Decision, kill switch
crates/tether-sandbox   Sandbox trait + backends
crates/tether-mcp       McpClient trait, sandbox server, rmcp-backed ExternalServer, Registry
crates/tether-models    ModelAdapter trait + adapters
crates/tether-core      run loop, state machine, checkpoint, resume, replay
crates/tether-cli       `tether` binary
crates/tetherd          gRPC daemon (v0.3)
proto/tether.proto      SDK service definition
sdks/, examples/        Python and TypeScript clients; runnable examples
ui/                     the trace viewer page, built into the binary
docs/                   PLAN, ARCHITECTURE, TRACE_FORMAT, REPLAY, RELEASING
```

Dependencies point down only. Leaf crates never import each other or
`tether-core`; they share types through `tether-types`.

## Working here

- `cargo check --workspace --all-targets` and `cargo test --workspace` must pass before any commit. `cargo clippy --workspace -- -D warnings` before a PR.
- Trace schema v0 is frozen. `crates/tether-trace/tests/compat.rs` reads traces written by v0.3.0; if it fails, the change is breaking. Never regenerate the fixtures to make it pass.
- `tether-types` changes are schema changes: update `docs/TRACE_FORMAT.md` in the same PR, and add or update a test in `crates/tether-types/tests/`.
- Tests never need an API key. Use the `fake` model adapter.
- Stop and ask before: changing the event envelope, adding a new event type, adding a dependency to `tether-types`, or adding a crate.
- No em dashes in prose or docs.
- Phases ship on a 4-week cadence and each is defined by its demo (see PLAN). Cut features before cutting the demo.

## Current phase

**v0.5 "it connects"** shipped 2026-11-22 as release v0.5.0. Next phase is
not yet planned; see `docs/PLAN.md` for the v1.0 roadmap items. Until a new
phase is written, work is issues only, and the launch items that waited on
MCP (the MCP community post, `mcp` back in topics and keywords) can go.
