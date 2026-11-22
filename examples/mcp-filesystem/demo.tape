# Renders docs/media/mcp.gif: the v0.5 demo against a real model and the
# reference filesystem MCP server.
#
# This makes billed API calls: one small task, a few cents. From the repo
# root, with ANTHROPIC_API_KEY set in the shell you run vhs from, and `npx`
# on that shell's PATH (the policy starts the server with it):
#
#   cargo build -p tether-cli -p tetherd
#   vhs examples/mcp-filesystem/demo.tape
#
# The key is read from the environment and never typed, so it is not in the
# GIF. It uses the SDK from sdks/python/.venv if that exists, else whatever
# `python` is on PATH. For a GIF with no key and no cost, drop
# `--model anthropic` from the agent's line.
#
# It empties examples/mcp-filesystem/.tether-mcp first, so the globs below
# match this run and no other, and makes the workspace the server is pointed
# at: the server will not start on a directory that is not there.

Output docs/media/mcp.gif
Set FontSize 13
Set Width 1300
Set Height 900

Hide
Type "export PATH=$PWD/target/debug:$PWD/sdks/python/.venv/bin:$PATH && cd examples/mcp-filesystem && rm -rf .tether-mcp && mkdir -p .tether-mcp/workspace && clear"
Enter
Sleep 2s
Show

# What the agent will see: the sandbox's tools and the server's, in one list.
Type "tether tools --policy tether.toml 2>/dev/null | cut -c1-100"
Enter
Sleep 10s

Type "clear"
Enter
# A real model: write with the sandbox, read through the server, then try to
# write through the server.
Type "python agent.py --model anthropic"
Enter
Sleep 60s

# Each call, the server it went to, and the rule that decided it.
Type "tether inspect .tether-mcp/run_*.jsonl | grep -E 'tool\.call|rule '"
Enter
Sleep 8s

# Replay answers from the recording: no model, no server, same run.
Type "tether replay .tether-mcp/run_*.jsonl --out .tether-mcp/replay.jsonl 2>&1 | head -2"
Enter
Sleep 4s
Type "tether diff .tether-mcp/run_*.jsonl .tether-mcp/replay.jsonl | tail -1"
Enter
Sleep 6s
