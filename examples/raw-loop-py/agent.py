"""A raw agent loop in Python, wrapped by Tether.

The Python twin of examples/raw-loop. The agent is `agent` below: ask the
model, run the tools it asks for, feed the results back, stop when it asks
for none. Its two call sites go through a `tether.Run` instead of a provider
SDK and a tool dispatcher, and that is the whole integration.

By default the model is scripted, so this needs no API key. `--model
anthropic` swaps in a real model and nothing else changes: the tools are
real either way and run in the sandbox.

    python agent.py
    ANTHROPIC_API_KEY=... python agent.py --model anthropic
    OPENAI_BASE_URL=http://localhost:11434/v1 python agent.py --model openai_compat <model name>
"""

import sys
from pathlib import Path

import tether

HERE = Path(__file__).resolve().parent
SYSTEM = (
    "You are working in a sandboxed workspace directory. "
    "Use the tools to do what is asked, then say what you did in one sentence."
)
TASK = "Write hello.txt, then show me what is in it."


def agent(run: tether.Run, model: str, params: dict) -> str:
    """The agent. Everything Tether-specific is the two `run.` calls."""
    messages = [tether.system(SYSTEM), tether.user(TASK)]
    while True:
        reply = run.complete(model=model, messages=messages, params=params)
        # The reply goes back into the history exactly as it came.
        messages.append(reply.message)
        results = [run.call_tool(call) for call in reply.tool_calls]
        # A refusal or a cut-off answer is a reply, but not a finished one.
        # The last two are what an OpenAI-style server calls the same things.
        if reply.stop_reason in ("refusal", "max_tokens", "content_filter", "length"):
            return f"stopped early ({reply.stop_reason}): {reply.text}"
        if not results:
            return reply.text
        messages.append(tether.tool_results(results))


def main() -> int:
    args = sys.argv[1:]
    if args in ([], ["--model", "fake"]):
        # What the scripted model does: write a file, read it back, report.
        settings = dict(provider="fake", script=HERE / "script.json")
        model, params = "fake-1", {}
    elif args == ["--model", "anthropic"]:
        # If the model declines a request, let the API retry it on another
        # model in the same call. That is a beta feature and needs its header.
        settings = dict(provider="anthropic", betas=["server-side-fallback-2026-07-01"])
        # A small task: ask for the least reasoning effort.
        model = "claude-opus-5-5"
        params = {"output_config": {"effort": "low"}, "fallbacks": "default"}
    elif len(args) == 3 and args[:2] == ["--model", "openai_compat"]:
        # Any server with an OpenAI-style chat completions endpoint, at
        # OPENAI_BASE_URL. The model is whatever that server calls it.
        settings = dict(provider="openai_compat")
        model, params = args[2], {}
    else:
        print(
            "usage: agent.py [--model fake|anthropic|openai_compat <model name>]",
            file=sys.stderr,
        )
        return 2

    # The file tools and a few read-only commands are allowed; anything else
    # asks, and with nobody answering an ask halts the run.
    with tether.Run(policy=HERE / "tether.toml", dir=HERE / ".tether", **settings) as run:
        print(f"trace: {run.trace_path}")
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
