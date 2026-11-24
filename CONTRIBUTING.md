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

`docs/PLAN.md` has the phases, and an ordered checklist when one is under way.
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

You need [rustup](https://rustup.rs). Nothing else is required for the Rust
workspace.

```sh
git clone https://github.com/chizy7/tether
cd tether
cargo test --workspace
```

`rust-toolchain.toml` names the Rust version that CI builds and lints with,
and rustup installs it the first time you run cargo here, so your clippy and
CI's agree. The oldest Rust that must still build Tether is 1.89.

The checks are also recipes in the `justfile`. [just](https://just.systems)
is optional: every recipe is a plain command, shown below. `just` on its own
lists them.

Tests never need an API key or a network connection. They use the `fake`
model adapter, which answers from a script. A new test should do the same.

## Before you open a PR

```sh
just check
```

which is:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

and a search of the repository for em dashes (see Conventions), which is
`just prose`.

CI runs these on Linux and macOS, and a few more depending on what you
touched:

| You changed | Also run |
| --- | --- |
| The Docker sandbox | `just docker` (needs Docker running) |
| `proto/tether.proto` | `just proto` and commit the result |
| The Python SDK, or any Python example | `pip install -e "sdks/python[dev]"` once, then `just python`: `ruff check .`, `ruff format --check .` and `python -m pytest sdks/python` |
| The TypeScript SDK | `npm ci` in `sdks/typescript` once, then `just typescript`: `npm run lint` and `npm test` there |
| `tether-types` | Update `docs/TRACE_FORMAT.md` and a test in `crates/tether-types/tests/` in the same PR |

`just python-fmt` and `just typescript-fmt` format the code and apply the
safe fixes. The Python settings are in `ruff.toml` and the TypeScript ones
in `sdks/typescript/biome.json`.

## Conventions

- **Dependencies point down.** Leaf crates never import each other or
  `tether-core`; they share types through `tether-types`. The crate graph is
  in `docs/ARCHITECTURE.md`.
- **PR titles** follow `feat:`, `fix:`, `docs:` or `chore:` and become the
  squash commit. A check on the PR holds the title to that; edit the title
  and it runs again.
- **One change per PR.** A fix and an unrelated cleanup are two PRs.
- **No em dashes** in prose, docs or comments. `just check` and CI fail on
  one anywhere in the repository.
- **Docs change with the code.** If behaviour a document describes changes,
  the document changes in the same PR.
- **The changelog changes with the code.** See below.

## The changelog

Every PR that someone using Tether would notice adds its own line to
`CHANGELOG.md`, and a check on the PR fails without one. The line you write
is what appears in the release notes, with your name on it.

- Put it under "Unreleased", in the section that fits: Added, Changed,
  Fixed, Removed or Security. Add the section heading if it is not there yet.
- Say what changed for the person using Tether, not how the code changed.
  Name the command, flag or file. Read a few existing entries for the tone.
- End with the PR number and, if you are not the maintainer, your GitHub
  name: `(#64, thanks @you)`. You know the number once the PR is open; push
  the line then.
- One line per entry, however long. Do not wrap it: a GitHub release shows
  every newline as a line break.

A change nobody using Tether would notice needs no entry: a dependency bump,
CI, a refactor with no change in behaviour, a typo. Say so in the PR and the
maintainer adds the `skip-changelog` label, which passes the check.

## Where to start

Issues labelled
[`good first issue`](https://github.com/chizy7/tether/labels/good%20first%20issue)
are real work that is small and well specified. Comment on one before
starting so two people do not do it twice.

## Code of conduct

Everyone taking part in the project is expected to follow the
[code of conduct](CODE_OF_CONDUCT.md). It says how to report a problem.

## Security

Do not open a public issue for a vulnerability. See `SECURITY.md`.

## License

Tether is licensed under Apache-2.0. By contributing you agree that your
contribution is licensed the same way.
