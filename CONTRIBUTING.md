# Contributing to Tether

Thanks for looking. Tether is small and has one maintainer, so the most useful
thing this page can do is tell you early whether a change will be merged.

## The one PR question

**Does this run agents, or build them?**

Tether is a harness. It runs an agent somebody else wrote: it records what the
agent does, decides what it may do, and plays it back. It does not help write
the agent. Prompt templates, retrieval, memory, agent-builder interfaces and
opinions about how a loop should be structured are out of scope, permanently,
however good the implementation is.

If you are unsure which side a change falls on, open an issue first and ask.

## Design rules

Every change is held to these five. A PR that breaks one needs a written
reason in its description.

1. **The trace is the only state.** A run can be rebuilt from its JSONL. No
   crate keeps state that is not also an event.
2. **Every side effect passes through policy.** Model calls, tool calls, file
   and network access are decided by `tether-policy` first, and the decision
   is recorded whether it was allow, deny or ask.
3. **Every tool is an MCP tool.** The sandbox is an MCP server named
   `sandbox`. One permission model covers everything.
4. **Nondeterminism is captured at the boundary.** Model responses, tool
   outputs, clock reads and random seeds are recorded on the way in, so replay
   can substitute them. Core code reads time and randomness through the
   `Clock` and `Rng` traits, never `std::time` or `rand` directly.
5. **The core has no opinion about the agent.** The agent emits model and tool
   requests; Tether answers them.

`docs/ARCHITECTURE.md` explains each in detail. Read it before touching
`tether-core`, `tether-trace` or `tether-types`.

## How a phase works

Tether ships in phases of about four weeks. Each phase is named for what it
proves ("it records", "it refuses", "it replays", "it evals") and is defined
by a demo: when the demo runs, the phase is done. If time runs short, features
are cut and the demo is kept.

`docs/PLAN.md` has the phases and an ordered checklist for the current one.
Work lands as one PR per checklist item. A feature that belongs to a later
phase waits for that phase, so check the plan before starting something large.

## Ask before you build

Open an issue and wait for an answer before:

- changing the event envelope or adding an event type
- changing anything in `tether-types` that alters the trace on disk
- adding a dependency to `tether-types`
- adding a crate to the workspace
- reopening a settled decision (the list is in `CLAUDE.md`)

The trace schema is frozen at v0. A change to the format on disk needs a
version bump and a reader for the old version. The test
`crates/tether-trace/tests/compat.rs` reads traces written by v0.3.0; if it
fails, the change is breaking. Do not regenerate its fixtures to make it pass.

## Setting up

You need Rust stable (1.89 or newer). Nothing else is required for the Rust
workspace.

```sh
git clone https://github.com/chizy7/tether
cd tether
cargo test --workspace
```

Tests never need an API key or a network connection. They use the `fake`
model adapter, which answers from a script. A new test should do the same.

## Before you open a PR

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs these, and a few more depending on what you touched:

| You changed | Also run |
| --- | --- |
| The Docker sandbox | `cargo test -p tether-sandbox --features docker` (needs Docker running) |
| `proto/tether.proto` | `sh sdks/python/scripts/gen_proto.sh` and commit the result |
| The Python SDK | `cargo build -p tetherd`, `pip install -e "sdks/python[dev]"`, `python -m pytest sdks/python` |
| The TypeScript SDK | `cargo build -p tetherd`, then `npm ci` and `npm test` in `sdks/typescript` |
| `tether-types` | Update `docs/TRACE_FORMAT.md` and a test in `crates/tether-types/tests/` in the same PR |

## Conventions

- **Dependencies point down.** Leaf crates never import each other or
  `tether-core`; they share types through `tether-types`. The crate graph is
  in `docs/ARCHITECTURE.md`.
- **PR titles** follow `feat:`, `fix:`, `docs:` or `chore:` and become the
  squash commit.
- **One change per PR.** A fix and an unrelated cleanup are two PRs.
- **No em dashes** in prose, docs or comments.
- **Docs change with the code.** If behaviour a document describes changes,
  the document changes in the same PR.

## Where to start

Issues labelled
[`good first issue`](https://github.com/chizy7/tether/labels/good%20first%20issue)
are real work that is small and well specified. Comment on one before
starting so two people do not do it twice.

## Security

Do not open a public issue for a vulnerability. See `SECURITY.md`.

## License

Tether is licensed under Apache-2.0. By contributing you agree that your
contribution is licensed the same way.
