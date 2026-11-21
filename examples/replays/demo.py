"""Record, replay, change the prompt, compare.

The v0.3 demo. For each of twenty small tasks:

1. **Record.** Run an agent on the task and keep its trace.
2. **Replay.** `tether replay` the trace as it is. No model call, no tool
   runs, no cost, and `tether diff` finds no divergence.
3. **Change the prompt.** `tether replay --system <new prompt>
   --live-after-divergence`. The new prompt is a request the recording has
   no answer to, so the run goes live from there, in a fresh workspace.
4. **Compare.** `tether diff` the recording against the new run, and read
   what changed off one table.

    python demo.py                     # scripted model, no API key
    python demo.py --model anthropic   # a real model; makes billed calls

Needs `tether` on PATH and the SDK installed. See README.md.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import tether

HERE = Path(__file__).resolve().parent

PROMPT = (
    "You are working in a sandboxed workspace directory. "
    "Use the tools to do what is asked, then say what you did in one sentence."
)
NEW_PROMPT = PROMPT + " Before you answer, read back every file you wrote to check it."

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


def scripted(task: dict, careful: bool) -> dict:
    """What the scripted model does for a task: write one file and say so.
    The careful version, standing in for the new prompt, reads it back first."""
    name = f"{task['id']}.txt"
    write = {"type": "tool_use", "id": "call_1", "name": "write_file",
             "arguments": {"path": name, "content": f"{task['task']}\n"}}
    read = {"type": "tool_use", "id": "call_2", "name": "read_file", "arguments": {"path": name}}
    replies = [[write]]
    if careful:
        replies.append([read])
        replies.append([{"type": "text", "text": f"Wrote {name} and checked it."}])
    else:
        replies.append([{"type": "text", "text": f"Wrote {name}."}])
    return {"task": task["task"], "replies": replies}


def record(task: dict, out: Path, real: bool) -> Path:
    """Run the agent on one task and return the path of its trace."""
    if real:
        settings = dict(provider="anthropic", betas=ANTHROPIC["betas"])
        model, params = ANTHROPIC["model"], ANTHROPIC["params"]
    else:
        script = out / f"{task['id']}.script.json"
        script.write_text(json.dumps(scripted(task, careful=False)))
        settings = dict(provider="fake", script=script)
        model, params = "fake-1", {}
    with tether.Run(policy=HERE / "tether.toml", dir=out, **settings) as run:
        try:
            run.finish(agent(run, model, params, task["task"]))
        except tether.Halted:
            pass  # The trace says why; the table will show it.
    return Path(run.trace_path)


def tether_cli(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["tether", *args], capture_output=True, text=True, stdin=subprocess.DEVNULL)


def replay(trace: Path, out: Path, *extra: str) -> None:
    done = tether_cli("replay", str(trace), "--out", str(out), *extra)
    # 1 means the replay left the recording and stopped, which is a result.
    if done.returncode not in (0, 1):
        sys.exit(f"tether replay {trace.name} failed:\n{done.stderr}")


def same(a: Path, b: Path) -> bool:
    """Whether `tether diff` finds no divergence between two traces."""
    done = tether_cli("diff", str(a), str(b))
    if done.returncode not in (0, 1):
        sys.exit(f"tether diff failed:\n{done.stderr}")
    return done.returncode == 0


def summary(trace: Path) -> dict:
    """What a run came to, read from its trace."""
    events = [json.loads(line) for line in trace.read_text().splitlines()]
    kinds = [event["type"] for event in events]
    spent = [e["data"]["usd_used"] for e in events if e["type"] == "budget.tick"]
    states = [e["data"] for e in events if e["type"] == "run.state"]
    outcome = [e["data"].get("outcome") for e in events if e["type"] == "run.finished"]
    return {
        "model_calls": kinds.count("model.response"),
        "tool_calls": kinds.count("tool.call"),
        "usd": spent[-1] if spent else 0.0,
        "ended": states[-1]["state"] if states else "?",
        "outcome": outcome[-1] if outcome else None,
    }


def cell(run: dict) -> str:
    ended = "" if run["ended"] == "finished" else f" {run['ended']}"
    return f"{run['model_calls']} calls, {run['tool_calls']} tools, ${run['usd']:.4f}{ended}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--model", choices=["fake", "anthropic"], default="fake")
    parser.add_argument("--tasks", type=int, default=20, help="how many of the tasks to run")
    args = parser.parse_args()
    real = args.model == "anthropic"
    if real and not (os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("ANTHROPIC_AUTH_TOKEN")):
        sys.exit("set ANTHROPIC_API_KEY to use --model anthropic")

    tasks = json.loads((HERE / "tasks.json").read_text())[: args.tasks]
    out = HERE / ".tether" / f"demo-{int(time.time())}"
    out.mkdir(parents=True)
    model = ANTHROPIC["model"] if real else "a scripted stand-in"
    print(f"model: {model}   tasks: {len(tasks)}   traces: {out}\n")

    rows = []
    for number, task in enumerate(tasks, 1):
        if sys.stdout.isatty():
            print(f"\r{number:>2}/{len(tasks)} {task['id']:<12}", end="", flush=True)
        recorded = record(task, out, real)

        # The trace as it is: nothing live, nothing spent.
        unchanged = recorded.with_suffix(".replay.jsonl")
        replay(recorded, unchanged)

        # The new prompt: live from the first request, in a fresh workspace.
        changed = recorded.with_suffix(".newprompt.jsonl")
        live = ["--system", NEW_PROMPT, "--live-after-divergence"]
        if not real:
            script = out / f"{task['id']}.careful.json"
            script.write_text(json.dumps(scripted(task, careful=True)))
            live += ["--script", str(script)]
        replay(recorded, changed, *live)

        rows.append({
            "task": task["id"],
            "recorded": summary(recorded),
            "replay_same": same(recorded, unchanged),
            "new": summary(changed),
            "new_same": same(recorded, changed),
        })
    if sys.stdout.isatty():
        print("\r" + " " * 40 + "\r", end="")

    def changed_how(row: dict) -> str:
        if row["new_same"]:
            return "nothing"
        extra = row["new"]["tool_calls"] - row["recorded"]["tool_calls"]
        tools = f"{extra:+d} tool calls" if extra else "same tool calls"
        answer = "same answer" if row["new"]["outcome"] == row["recorded"]["outcome"] else "new answer"
        return f"{tools}, {answer}"

    header = f"{'task':<10} {'recorded':<28} {'replayed':<9} {'with the new prompt':<28} what changed"
    print(header)
    print("-" * (len(header) + 14))
    for row in rows:
        replayed = "same" if row["replay_same"] else "DIVERGED"
        print(
            f"{row['task']:<10} {cell(row['recorded']):<28} {replayed:<9} "
            f"{cell(row['new']):<28} {changed_how(row)}"
        )
    print("-" * (len(header) + 14))

    identical = sum(row["replay_same"] for row in rows)
    more = sum(row["new"]["tool_calls"] > row["recorded"]["tool_calls"] for row in rows)
    diverged = sum(not row["new_same"] for row in rows)
    recorded_usd = sum(row["recorded"]["usd"] for row in rows)
    new_usd = sum(row["new"]["usd"] for row in rows)
    print(f"replayed unchanged: {identical} of {len(rows)} identical to their recording, at no cost")
    print(f"with the new prompt: {diverged} of {len(rows)} diverged, {more} made more tool calls")
    print(f"spent: ${recorded_usd:.4f} recording, ${new_usd:.4f} on the new prompt")

    # A replay of an unchanged trace that diverges is a bug in Tether.
    return 0 if identical == len(rows) else 1


if __name__ == "__main__":
    sys.exit(main())
