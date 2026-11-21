# examples/replays

The v0.3 demo: record twenty small tasks, replay them, change the prompt,
replay again, and read what changed off one table.

For each task in `tasks.json`, `demo.py`:

1. **Records** an agent doing the task, through the Python SDK.
2. **Replays** the trace as it is with `tether replay`. No model call, no
   tool runs, no cost. `tether diff` against the recording finds nothing.
3. **Changes the prompt** with `tether replay --system <new prompt>
   --live-after-divergence`. The recording has no answer to a request with a
   different system prompt, so the run goes live from its first request, in
   a fresh workspace.
4. **Compares** the two with `tether diff` and prints a row.

The new prompt adds one sentence to the old: "Before you answer, read back
every file you wrote to check it."

## Run it

From the repo root, once:

```
cargo build -p tether-cli -p tetherd
pip install -e sdks/python
export PATH="$PWD/target/debug:$PATH"
```

Then:

```
python examples/replays/demo.py                     # scripted model, no API key
python examples/replays/demo.py --model anthropic   # Claude Opus 5.5
```

`--tasks N` runs only the first N. Traces go to
`examples/replays/.tether/demo-<time>/`, three per task: `<run>.jsonl`,
`<run>.replay.jsonl` and `<run>.newprompt.jsonl`.

With `--model anthropic` this makes real, billed API calls: about three per
task to record and about four per task again under the new prompt, which
runs live. Expect well under a dollar for all twenty. `tether.toml` caps each
run at $0.15, 15 tool calls and two minutes, allows the file tools and a few
read-only commands, and halts on anything else. The requests ask for low
reasoning effort and turn on the API's server-side fallback (a beta feature);
both are at the top of `demo.py`.

## Reading the table

```
task       recorded                     replayed  with the new prompt          what changed
hello      3 calls, 2 tools, $0.0125    same      4 calls, 3 tools, $0.0171    +1 tool calls, new answer
```

- **recorded**: model calls, tool calls and cost of the original run.
- **replayed**: `same` when the replay of the unchanged trace does not
  diverge from it. Anything else is a bug in Tether, and the script exits 1.
- **with the new prompt**: the same three figures for the live run.
- **what changed**: the difference in tool calls and whether the final
  answer is word for word the same.

The scripted model writes one file per task, and reads it back when it
stands in for the new prompt, so its table is the same in every row. A real
model's is not.
