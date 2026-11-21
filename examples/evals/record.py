"""Record the suite that `tether eval` runs.

The v0.4 demo, first half. Twenty small tasks are run once, under the prompt
in prompt.txt, and each trace is packed into suite/<task>.tar. That is the
suite: twenty recordings and the assertions in suite/eval.toml.

Then `tether eval` does the rest, with no Python involved:

    tether eval suite                                       # the recordings pass
    tether eval suite --system "$(cat regressed.txt)"       # a worse prompt does not

    python record.py                     # scripted model, no API key
    python record.py --model anthropic   # a real model; makes billed calls

Needs `tether` on PATH and the SDK installed. See README.md.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import tether

HERE = Path(__file__).resolve().parent
PROMPT = (HERE / "prompt.txt").read_text().strip()

ANTHROPIC = dict(
    model="claude-opus-5-5",
    # A small task: the least reasoning effort. And if the model declines a
    # request, let the API retry it on another model in the same call, which
    # is a beta feature and needs its header.
    params={"output_config": {"effort": "low"}, "fallbacks": "default"},
    betas=["server-side-fallback-2026-07-01"],
)


def agent(run: tether.Run, model: str, params: dict, task: str) -> str:
    """The agent: a plain tool loop."""
    messages = [tether.system(PROMPT), tether.user(task)]
    while True:
        reply = run.complete(model=model, messages=messages, params=params)
        messages.append(reply.message)
        results = [run.call_tool(call) for call in reply.tool_calls]
        if reply.stop_reason in ("refusal", "max_tokens"):
            return f"stopped early ({reply.stop_reason}): {reply.text}"
        if not results:
            return reply.text
        messages.append(tether.tool_results(results))


def scripted(task: dict, tidy: bool) -> dict:
    """What the scripted model does for a task: write its files, one call a
    turn, then say so. The tidy version stands in for the model under
    regressed.txt: where it made a scratch file, it tries to delete it."""
    calls = [["write_file", {"path": path, "content": content}] for path, content in task["writes"].items()]
    calls += task.get("also", [])
    if tidy and "scratch" in task:
        calls.append(["exec", {"argv": ["rm", task["scratch"]]}])
    replies = [
        [{"type": "tool_use", "id": f"call_{n}", "name": name, "arguments": arguments}]
        for n, (name, arguments) in enumerate(calls, 1)
    ]
    replies.append([{"type": "text", "text": task["says"]}])
    return {"task": task["task"], "replies": replies}


def record(task: dict, out: Path, real: bool) -> Path:
    """Run the agent on one task and return the path of its trace."""
    if real:
        settings = dict(provider="anthropic", betas=ANTHROPIC["betas"])
        model, params = ANTHROPIC["model"], ANTHROPIC["params"]
    else:
        script = out / f"{task['id']}.script.json"
        script.write_text(json.dumps(scripted(task, tidy=False)))
        settings = dict(provider="fake", script=script)
        model, params = "fake-1", {}
    with tether.Run(policy=HERE / "tether.toml", dir=out, **settings) as run:
        try:
            run.finish(agent(run, model, params, task["task"]))
        except tether.Halted:
            pass  # The trace says why, and `tether eval` will report it.
    return Path(run.trace_path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--model", choices=["fake", "anthropic"], default="fake")
    args = parser.parse_args()
    real = args.model == "anthropic"
    if real and not (os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("ANTHROPIC_AUTH_TOKEN")):
        sys.exit("set ANTHROPIC_API_KEY to use --model anthropic")

    tasks = json.loads((HERE / "tasks.json").read_text())
    suite, scripts, out = HERE / "suite", HERE / "scripts", HERE / ".tether"
    # Start clean: yesterday's recordings and results are not today's suite.
    for old in [*suite.glob("*.tar"), suite / ".eval", scripts, out]:
        shutil.rmtree(old) if old.is_dir() else old.unlink(missing_ok=True)
    out.mkdir()

    model = ANTHROPIC["model"] if real else "a scripted stand-in"
    print(f"recording {len(tasks)} tasks with {model}")
    for number, task in enumerate(tasks, 1):
        if sys.stdout.isatty():
            print(f"\r{number:>2}/{len(tasks)} {task['id']:<12}", end="", flush=True)
        trace = record(task, out, real)
        packed = subprocess.run(
            ["tether", "pack", str(trace), "--out", str(suite / f"{task['id']}.tar")],
            capture_output=True, text=True,
        )
        if packed.returncode != 0:
            sys.exit(f"\ntether pack {trace.name} failed:\n{packed.stderr}")
    if sys.stdout.isatty():
        print("\r" + " " * 40 + "\r", end="")

    here = os.path.relpath(HERE)
    live = f'tether eval {here}/suite --system "$(cat {here}/regressed.txt)"'
    if not real:
        # With no real model to ask, a script per task says what it would do.
        scripts.mkdir()
        for task in tasks:
            (scripts / f"{task['id']}.json").write_text(json.dumps(scripted(task, tidy=True)))
        live += f" --scripts {here}/scripts"
    print(f"the suite is in {here}/suite: {len(tasks)} recordings and eval.toml\n")
    print("check the recordings against the suite's assertions (no model, no cost):")
    print(f"  tether eval {here}/suite\n")
    print("then try a prompt that tells the agent to tidy up after itself:")
    print(f"  {live}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
