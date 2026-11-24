<!-- Title: feat:, fix:, docs: or chore:, then what changed. It becomes the squash commit. -->

**What this does**


**Why**


**Checks**

- [ ] `just check` (format, clippy, tests; the commands are in CONTRIBUTING.md)
- [ ] If an SDK or a Python example changed: `just python` or `just typescript`
- [ ] Tests need no API key (they use the `fake` adapter)
- [ ] Docs that describe the changed behaviour are updated
- [ ] If `tether-types` changed: `docs/TRACE_FORMAT.md` and a test in `crates/tether-types/tests/` are updated, and `crates/tether-trace/tests/compat.rs` still passes without touching its fixtures

**Changelog**

Add a line for this under "Unreleased" in `CHANGELOG.md`. If nobody using Tether would notice the change (a dependency bump, CI, a refactor, a typo), tick this instead:

- [ ] No changelog entry needed

**Design rules**

If this breaks one of the five design rules in CONTRIBUTING.md, say which and why.
