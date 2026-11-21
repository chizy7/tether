# tether-agents

TypeScript client for [Tether](https://github.com/chizy7/tether/tree/master/README.md). Your agent keeps its own loop;
it calls `run.complete` where it called its provider SDK and `run.callTool`
where it ran a tool, and Tether does the rest: policy, sandbox, budgets and
the trace.

```ts
import { Run, toolResults, user } from "tether-agents";

const run = await Run.start({ policy: "tether.toml" });
const messages = [user("Write hello.txt, then show me what is in it.")];
for (;;) {
  const reply = await run.complete({ model: "claude-opus-5-5", messages });
  messages.push(reply.message);
  if (reply.toolCalls.length === 0) {
    await run.finish(reply.text);
    break;
  }
  const results = [];
  for (const call of reply.toolCalls) results.push(await run.callTool(call));
  messages.push(toolResults(results));
}
run.close();
```

A complete example is in [examples/raw-loop-ts](https://github.com/chizy7/tether/tree/master/examples/raw-loop-ts).

## Install

```
npm install tether-agents
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/chizy7/tether/releases/latest/download/tether-cli-installer.sh | sh
```

The second line installs the `tether` binary, which carries the daemon the
SDK talks to. The SDK and the binary should be the same version. Node 20 or
later, on macOS or Linux.

## How it works

The SDK is a thin client for the daemon, `tetherd`, over gRPC on a Unix
socket. Each method is one request and nothing in the SDK decides anything.
It is the same surface as the Python SDK, with JavaScript's spelling.

`Run.start(...)` connects to the socket and starts a run. If no daemon
answers, it starts one: `$TETHERD` if set, else `tetherd` or `tether daemon`
from `PATH`. The daemon logs to `tetherd.log` beside the socket and exits by
itself a few seconds after its last run.

| | |
| --- | --- |
| `Run.start({ policy, provider, script, dir, workspace, agent, betas, socket })` | Start a run. `provider` is `"anthropic"`, `"openai_compat"` (any server with an OpenAI-style `/chat/completions`, set by `OPENAI_BASE_URL` and `OPENAI_API_KEY`) or, with `script`, `"fake"` |
| `run.tools` | The tools the run offers |
| `run.complete({ model, messages, tools, params })` | One model turn. Resolves to a `Reply` |
| `run.callTool(call)` | Run a tool the model asked for. Resolves to a `ToolResult` |
| `run.finish(outcome)` | The agent is done |
| `run.halt()` | Trip the run's kill switch |
| `run.answer(subjectSeq, allow)` | Answer a policy ask, while the call that asked is pending |
| `run.events()` | Iterate the trace as it is written, with `for await` |
| `run.id`, `run.tracePath` | The run's id and where its trace is |
| `run.close()` | Close the connection |
| `withRun(options, async (run) => ...)` | Start a run, hand it to the function, then finish it, or halt it if the function threw, and close |

Things to know:

- Append `reply.message` to your history exactly as it is. It can hold blocks
  only the provider understands, such as reasoning blocks, which must go back
  unchanged.
- Check `reply.stopReason` before acting. `refusal` and `max_tokens` are
  replies that are not finished answers.
- A tool call that policy denies does not throw. It resolves to a
  `ToolResult` with `isError` set, like any other failed call.
- `Halted` is thrown when the run takes no more calls: a spent budget, an
  unanswered ask, the kill switch, a provider failure. Anything else the
  daemon refuses throws `TetherError` with the gRPC status in `.code`.
- Options left out fall back to `TETHER_POLICY`, `TETHER_DIR` and
  `TETHER_SOCKET`, which `tether run -- node agent.js` sets.
- The package is CommonJS and has its own type declarations. It can be
  imported from an ES module.

## Develop

```
cd sdks/typescript
npm install
cargo build -p tetherd
npm test
```

The service definition is `proto/tether.proto` at the root of the repository.
The build copies it into the package, and the client loads it when it starts,
so there is no generated code to keep in step.
