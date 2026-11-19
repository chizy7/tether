# examples/refuses

The agent asks to run `rm -rf .`, policy blocks it, and the trace shows the
decision and the rule that made it.

- `script.json` is the agent: a task and the model's replies, played by the
  `fake` adapter, so no API key is needed. It saves a note, tries to delete
  the workspace, then lists what is left.
- `tether.toml` is the policy. Its first rule, `no-rm`, denies any
  `sandbox.exec` whose command is `rm`.

```
cargo build -p tether-cli
cd examples/refuses
../../target/debug/tether run --script script.json --policy tether.toml
../../target/debug/tether inspect .tether/run_*.jsonl
```

The run finishes: a denied call comes back to the agent as an ordinary failed
tool call, and it carries on. In the trace the blocked call is three events:

```
tool.call        exec {"argv":["rm","-rf","."]}
policy.decision  deny: deleting files is off limits in this run (seq 14, rule no-rm)
tool.result      error, 0 ms, "denied by policy: deleting files is off limits in this run"
```

`notes.txt` is still in `.tether/workspace` afterwards.

Change `action = "deny"` to `"ask"` in `tether.toml` and the run stops at a
prompt instead, waiting for `y` or `n`.

The rule matches the command name, so it does not see `sh -c "rm -rf ."`.
A real policy denies by default and allows narrowly.
