# examples/mcp-filesystem

The v0.5 demo: one external MCP server beside the built-in sandbox tools.

![tether tools lists the sandbox tools and an MCP filesystem server's; a real model's write through the server is denied by a rule; the replay does not diverge](../../docs/media/mcp.gif)

The GIF is the run against Claude Opus 5.5 (`agent.py --model anthropic`,
below): four model calls, three tool calls, about seven cents.

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
child `PATH` and the variables the policy's `env` list names, nothing else.

## With a real model

`agent.py` is the same run through the Python SDK, so the model can be a real
one. It is a plain tool loop; the policy and the server are the ones above.

```
python agent.py                                       # scripted model, no API key
ANTHROPIC_API_KEY=... python agent.py --model anthropic
```

The second makes real, billed calls to Claude Opus 5.5: one small task, a few
cents. The policy caps the run at $0.50, 20 tool calls and five minutes, and
allows only the sandbox's file tools: `exec` matches no rule, so it asks, and
with nobody answering the run halts.

`demo.tape` renders `docs/media/mcp.gif` from that run; the commands are at
the top of the file.

Two things worth knowing when you write your own policy:

- Rules see every tool as `<server>.<tool>`, whatever name the agent used. The
  server's `read_text_file` is `fs.read_text_file` to a rule.
- Where a server's tool name collides with a sandbox tool (`read_file`,
  `write_file`), the agent sees the external one as `fs__read_file`; the
  sandbox keeps the bare name.
