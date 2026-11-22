# examples/mcp-filesystem

The v0.5 demo: one external MCP server beside the built-in sandbox tools.

`tether.toml` names the reference filesystem server
(`@modelcontextprotocol/server-filesystem`, started over stdio with `npx`) and
points it at the run's workspace. The scripted agent then:

1. writes `hello.txt` with the sandbox's `write_file`
2. reads it back with the server's `read_text_file`, which a rule allows
3. tries to overwrite it with the server's `write_file`, which a rule denies

Both external calls are in the trace with `server: "fs"`, the rule that
decided them, and `external: true`. Replaying the trace answers them from the
recording and starts no server.

```
cargo build -p tether-cli
cd examples/mcp-filesystem
mkdir -p .tether-mcp/workspace

tether tools --policy tether.toml                  # what the agent would see, by server
tether run --script script.json --policy tether.toml --dir .tether-mcp
tether inspect .tether-mcp/<run>.jsonl
tether replay  .tether-mcp/<run>.jsonl             # no npx, no server, no network
tether diff    .tether-mcp/<run>.jsonl .tether-mcp/<run>.replay.jsonl
```

Needs `node` and `npx` on `PATH` for the server itself; Tether passes the
child only the variables the policy's `env` list names.

Two things worth knowing when you write your own policy:

- Rules see every tool as `<server>.<tool>`, whatever name the agent used. The
  server's `read_text_file` is `fs.read_text_file` to a rule.
- Where a server's tool name collides with a sandbox tool (`read_file`,
  `write_file`), the agent sees the external one as `fs.read_file`; the
  sandbox keeps the bare name.
