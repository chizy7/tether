"""An agent with a real model, the sandbox tools and an external MCP server.

The same run as `tether run --script script.json`, but through the Python SDK,
so the model can be a real one. The agent is a plain tool loop; its two call
sites go through a `tether.Run`. The policy in tether.toml names the reference
filesystem server, lets the agent read through it, and refuses writes through
it.

    python agent.py                                   # scripted model, no API key
    ANTHROPIC_API_KEY=... python agent.py --model anthropic

Needs `tether` on PATH, the SDK installed, and `npx` for the server.
"""

import os
import shutil
import sys
from pathlib import Path

import tether

HERE = Path(__file__).resolve().parent
RUNS = HERE / ".tether-mcp"
SYSTEM = (
    "You work in a sandboxed workspace directory. You have two sets of file "
    "tools on the same directory: the sandbox's own (write_file, read_file, "
    "list_dir) and a filesystem server's (read_text_file, fs__write_file and "
    "others). Do exactly the steps you are given, with exactly the tool each "
    "names, one tool call at a time. If a call is refused, do not retry it or "
    "work around it. Finish with one sentence saying what happened at each step."
)
TASK = (
    "1. Write hello.txt containing 'hello from the sandbox' with write_file. "
    "2. Read hello.txt back with read_text_file. "
    "3. Overwrite hello.txt with 'overwritten' using fs__write_file."
)


def agent(run: tether.Run, model: str, params: dict) -> str:
    """The agent. Everything Tether-specific is the two `run.` calls."""
    messages = [tether.system(SYSTEM), tether.user(TASK)]
    while True:
        reply = run.complete(model=model, messages=messages, params=params)
        messages.append(reply.message)
        results = [run.call_tool(call) for call in reply.tool_calls]
        if reply.stop_reason in ("refusal", "max_tokens"):
            return f"stopped early ({reply.stop_reason}): {reply.text}"
        if not results:
            return reply.text
        messages.append(tether.tool_results(results))


def main() -> int:
    args = sys.argv[1:]
    if args in ([], ["--model", "fake"]):
        settings = dict(provider="fake", script=HERE / "script.json")
        model, params = "fake-1", {}
    elif args == ["--model", "anthropic"]:
        if not (os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("ANTHROPIC_AUTH_TOKEN")):
            sys.exit("set ANTHROPIC_API_KEY to use --model anthropic")
        # If the model declines a request, let the API retry it on another
        # model in the same call. That is a beta feature and needs its header.
        settings = dict(provider="anthropic", betas=["server-side-fallback-2026-07-01"])
        # A small task: ask for the least reasoning effort.
        model = "claude-opus-5-5"
        params = {"output_config": {"effort": "low"}, "fallbacks": "default"}
    else:
        print("usage: agent.py [--model fake|anthropic]", file=sys.stderr)
        return 2

    # The policy gives the server a path relative to here, and the daemon the
    # SDK starts inherits this directory.
    os.chdir(HERE)
    # One workspace, shared by the sandbox tools and the server, emptied so
    # each run starts the same.
    workspace = RUNS / "workspace"
    shutil.rmtree(workspace, ignore_errors=True)
    workspace.mkdir(parents=True)

    with tether.Run(
        policy=HERE / "tether.toml", dir=RUNS, workspace=workspace, **settings
    ) as run:
        print(f"trace: {os.path.relpath(run.trace_path)}")
        try:
            answer = agent(run, model, params)
        except tether.Halted as halted:
            print(f"halted: {halted}", file=sys.stderr)
            return 1
        run.finish(answer)
    print(answer)
    return 0


if __name__ == "__main__":
    sys.exit(main())
