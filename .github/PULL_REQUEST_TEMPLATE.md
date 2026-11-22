<!-- Title: feat:, fix:, docs: or chore:, then what changed. It becomes the squash commit. -->

**What this does**


**Why**


**Checks**

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Tests need no API key (they use the `fake` adapter)
- [ ] Docs that describe the changed behaviour are updated
- [ ] If `tether-types` changed: `docs/TRACE_FORMAT.md` and a test in `crates/tether-types/tests/` are updated, and `crates/tether-trace/tests/compat.rs` still passes without touching its fixtures

**Design rules**

If this breaks one of the five design rules in CONTRIBUTING.md, say which and why.
