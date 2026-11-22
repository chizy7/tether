# Replaying an agent run exactly, and what it takes

[Tether](https://github.com/chizy7/tether) is a Rust harness that wraps an
agent somebody else wrote. Every model call and tool call the agent makes
goes through it, is checked against a policy, and is written to a trace. This
is about one property of that trace: you can run the agent again from it,
with no network and no cost, and get the same run. Not a similar run. The
same events, equal field for field.

That sounds like a logging feature. It turned out to be the design decision
everything else hangs from, and getting it exactly right took more care than
expected. This write-up covers why it is worth having, the four decisions
that make it work, and where it stops working.

## What you get

Here is a recorded run of a small agent that writes a note, tries to delete
its workspace and is refused. Replaying it and comparing the two:

```
$ tether replay run.jsonl --out replay.jsonl
every request was answered from the recording

$ tether diff run.jsonl replay.jsonl
step  requests  tool calls  decisions  (a/b)
   1       1/1         1/1        2/2  same
   2       1/1         1/1        2/2  same
   3       1/1         1/1        2/2  same
   4       1/1         0/0        1/1  same

no divergence
```

On its own that is a party trick. It becomes useful when you change one
thing and replay again, because whatever differs is then caused by the thing
you changed.

**Change the policy.** Policy is the one part of a run that is not played
back. It is evaluated again, so you can hold an old run to new rules and see
what they would have decided, without the agent doing anything:

```
$ tether replay run.jsonl --policy strict.toml --out strict.jsonl
$ tether diff run.jsonl strict.jsonl
the two runs were held to different policies

first divergence: step 1, policy.decision: the outcome differs
  a (seq 7): allow (rule write-workspace)
  b (seq 7): deny (rule write-workspace: denied by rule write-workspace)
```

A week of production traces is a test suite for a policy change, and running
it costs nothing.

**Change the prompt or the model.** Now the recording cannot answer, because
the model was never asked this question. The replay follows the recording up
to the first request that differs, then goes live: real model, real tools, a
fresh workspace. Everything before that point is free and identical, and
`diff` shows the exact request where the two runs part.

**Do that over a suite.** `tether eval` is this, twenty times, plus
assertions over each new trace: did it finish, did it stay under a cost, was
anything refused, did it write the file. In the example in the repo, twenty
tasks recorded against a real model all pass against their own recordings.
Asked again under a
prompt that tells the agent to tidy up after itself, one fails, because the
agent tried `rm` and the policy refused it. The table names the rule.

There is no eval engine in Tether. Eval is replay, diff and a table. Resume
after a crash is replay up to the last checkpoint, then live. The trace
viewer and the OpenTelemetry export read the same file. None of them needed
new state, and that is the first decision.

## Decision 1: the trace is the only state

A run in Tether is one append-only file, one JSON event per line. The rule
is that no part of the system holds anything that is not also an event in
that file. Budgets spent, the state of the run, a question waiting for a
human: if it matters, it is in the trace.

The rule is easy to state and annoying to keep. A cache of the message
history in memory is state. A counter of dollars spent is state. Each one is
fine as long as it can be rebuilt from the file, and a bug the moment it
cannot.

What the rule buys is that every feature after it is a reader:

- **Resume** reads the trace to the last checkpoint and carries on, appending
  to the same file. There is no separate checkpoint format. A checkpoint is
  an event that says "the trace up to here is a consistent place to start
  from".
- **Replay** reads the trace and answers the agent from it.
- **Diff** reads two.
- **Eval** reads the output of replay.

It also means a run is one thing you can hand to someone. `tether pack` puts
the file and its payloads in a tar, and every command reads the tar in place.

Each event carries the BLAKE3 hash of the one before it, computed by the
single task that writes the file. That catches truncation and edits in
place, which is what goes wrong by accident. It is not a signature: someone
who can rewrite the whole file can rewrite the chain.

## Decision 2: capture nondeterminism at the boundary

For a replay to be exact, everything the run learned from outside has to be
in the trace, recorded on the way in. There are four sources:

| Source | Recorded as | Under replay |
| --- | --- | --- |
| The model | `model.response`, with its price, or the provider's error | Given back, matched on the hashes of the request's messages |
| Tools | `tool.result` | Given back, matched on tool name and arguments. The tool does not run |
| A human answering an ask | `policy.answer` | Given back, in order |
| The clock | Timestamps and durations on the events | Restored (see decision 4) |

The agent does not know which it is talking to. In a replay it makes the
same two calls it always makes, and what answers is a stand-in built from the
recording instead of a provider and a sandbox.

Two details mattered more than they looked.

**Requests are matched by content, not by position.** A replayed model
request is looked up by the hashes of its messages. If the agent sends
exactly what it sent before, it gets exactly what it got before. If it sends
anything else, there is no entry, and that is the definition of divergence.
Matching "the third request" by position would happily hand a recorded
answer to a different question.

**A refused call has no result to replay.** If the recording denied a tool
call, the tool never ran, so there is nothing recorded for it. Replay under
a looser policy that now allows the call therefore diverges at that point:
it needs something the recording does not have. Replay under a stricter
policy does not diverge at all, because refusing a call needs nothing from
outside. That asymmetry is why the policy example above runs to the end.

## Decision 3: one blob per message

An agent's model request contains the whole conversation so far. Step 1
sends one message, step 2 sends three, step 40 sends about eighty. Written out
naively, a trace stores the first message forty times, and a long run's
trace grows with the square of its length.

So a `model.request` event does not contain messages. It contains a list of
hashes:

```
step 1  "messages": ["blake3:4d51…"]
step 2  "messages": ["blake3:4d51…", "blake3:258e…", "blake3:68fa…"]
```

Each message is stored once, in a directory beside the trace, under the
BLAKE3 hash of its content. A step adds the new messages and nothing else.
The history is never copied.

This was done for size. It paid off somewhere else. The list of hashes is
exactly the key replay needs: two requests carry the same conversation if and
only if their lists are equal, and comparing them is comparing a few short
strings. Diff uses the same lists. When two runs part, the first hash that
differs names the message that changed, and `diff` prints both versions from
the two blob stores. Storing per message is why the tool can say "message 1
of 2 differs" and not just "the request differs".

Large tool results get the same treatment: anything over 32 KB goes to the
blob store and the event keeps the hash.

## Decision 4: restore timings, do not measure them

This is the one with two tempting wrong answers.

A trace is full of times. Every event has a timestamp. A model response has
a latency, a tool result a duration, and the budget events carry how many
seconds of wall clock the run has used. All of them come from reading the
clock, and a replay that reads the clock gets different numbers every time.

The easy fix is to tell `diff` to ignore those fields. That hides the
symptom and leaves a real bug. A run can halt because it ran out of its
wall-clock budget. Replay it with live timing and it finishes in forty
milliseconds, never reaches the limit, and carries on past the point where
the original stopped. The replay is now a different run.

The second idea is a replay clock that hands back the recorded readings in
order. That works until someone changes the run loop to read the clock one
more time, or one fewer. Then every reading after it is off by one, silently.
It ties the correctness of every old trace to the exact sequence of clock
reads in the code that wrote it.

What Tether does: as the replay writes each event, it finds that event's
counterpart in the recording, the next recorded event of the same kind
within the same call, and copies its timestamp and timing fields. The budget
check before each call uses the recorded elapsed time, so a run that halted
on wall clock halts at the same place. Nothing depends on how many times the
loop looks at the clock.

The result is stronger than "diff finds nothing". A replay's events are
equal to the recording's in every field except the run id and the hash
chain.

After a divergence the clock is live again. Elapsed time carries on from the
recorded figure, and timestamps jump from the recording's to now.

## Where determinism breaks

A replay is exact for as long as the agent asks the same questions. Here is
what ends that, and what happens when it does.

**The agent itself is nondeterministic.** If the agent shuffles a list or
puts the current date in a prompt, its second run sends a different request
and the replay diverges there. Tether cannot fix this and does not try. It
reports it, which is often useful: a replay that diverges with nothing
changed has found nondeterminism in the agent. There is a second way to
replay that avoids the question, which is that `tether replay` does not run
the agent at all. It re-issues the recorded requests and tool calls in
order, so the recording plays back whatever the agent's code does today.

**You changed something on purpose.** This is the useful case. The replay
stops at the first request it cannot answer and exits 1, or with
`--live-after-divergence` it carries on against the real model:

```
$ tether replay run.jsonl --system "Be brief."
left the recording and stopped: model request has no recorded response:
message 1 of 2 differs from the recording
```

**Going live needs the workspace to be where the recording left it.** Up to
the divergence no tool has actually run, so the sandbox is empty. Before the
first live tool call, Tether runs again the calls that had been replayed, in
a fresh workspace, never the original's. That rebuilds the files.

**Some tools must never run twice.** Rebuilding a workspace by running
`write_file` again is harmless. Sending an email again is not. A tool marked
`external` always gives back its recorded result and is never invoked again,
before or after a divergence.

**Request headers are not in the trace.** The trace records the request
body. Going live with a recorded request that depends on a header needs the
header added back, and today that is done by name for the one case that has
come up.

## What it does not do

- **It replays the run, not the world.** A tool result is given back as it
  was recorded. If a command read a file that has since changed, the replay
  sees the old content. That is the point, and it is also why a replay tells
  you nothing about whether the run would work today. Going live does.
- **A trace holds what the agent saw.** Prompts, responses and tool output
  are stored in full and unencrypted. If a tool read a secret, it is in
  there.
- **Diff lines events up by position within a step.** One extra tool call
  shifts everything after it in that step. The first divergence is the one
  to read; after it the runs are doing different things.
- **Assertions see the trace, nothing else.** An eval can check what
  `write_file` wrote, because the content is in the event. It cannot check a
  file made by a shell command, because only the command and its output are
  recorded.
- **A model changing under the same name is invisible to replay,** which is
  exact by construction. It shows up the moment you go live, and comparing
  the two runs is how you find out.

## What it cost

A whole-trace read runs at about 200,000 events a second, so `inspect`,
`verify` and `diff` on a 100,000-event trace each take about half a second.
There is no index; one was planned, measured and found unnecessary.

The larger cost is discipline. Every new feature has to answer "where is
this in the trace?" before it is written, and every clock read and random
number in the core goes through a trait so it can be substituted. In return,
replay, resume, diff, eval, the viewer and the export are small programs
that read a file.

## Try it

No API key is needed; the examples come with a scripted model.

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/chizy7/tether/releases/latest/download/tether-cli-installer.sh | sh
git clone https://github.com/chizy7/tether
cd tether/examples/refuses
tether run --script script.json --policy tether.toml
tether replay .tether/run_*.jsonl --out .tether/replay.jsonl
tether diff .tether/run_*.jsonl .tether/replay.jsonl
```

The trace format is specified in [TRACE_FORMAT.md](TRACE_FORMAT.md) and is
frozen at v0. The mechanics above are in
[ARCHITECTURE.md](ARCHITECTURE.md) under "Checkpoint, resume, replay".

One honest note to end on: the default sandbox runs commands as ordinary
child processes and is not a security boundary. The
[README](../README.md#honest-note-on-sandboxing) says exactly what it does
and does not stop.
