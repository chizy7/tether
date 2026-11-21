// The SDK against a real daemon, with the scripted model. No API key.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, test } from "node:test";

import { status } from "@grpc/grpc-js";

import * as tether from "../src/index";

// From dist/test, the repository root is four levels up.
const REPO = path.resolve(__dirname, "..", "..", "..", "..");

const SCRIPT = {
  replies: [
    [
      {
        type: "tool_use",
        id: "call_1",
        name: "write_file",
        arguments: { path: "notes.txt", content: "from typescript\n" },
      },
    ],
    [{ type: "tool_use", id: "call_2", name: "exec", arguments: { argv: ["rm", "notes.txt"] } }],
    [{ type: "text", text: "Done." }],
  ],
};

const policy = (action: string) => `
[[rule]]
name = "writes"
match = { tool = "sandbox.write_file" }
action = "allow"

[[rule]]
name = "no-rm"
match = { tool = "sandbox.exec", argv0 = "rm" }
action = "${action}"
`;

/** A short-lived directory with a script and two policies. Short path: sockets have a length limit. */
let where = "";
const saved: Record<string, string | undefined> = {};
const VARIABLES = ["TETHERD", "TETHER_SOCKET", "TETHER_DIR", "TETHER_POLICY", "PATH"];

beforeEach(() => {
  for (const name of VARIABLES) saved[name] = process.env[name];
  const daemon = process.env.TETHERD || path.join(REPO, "target", "debug", "tetherd");
  assert.ok(fs.existsSync(daemon), `${daemon} is not built; run \`cargo build -p tetherd\` first`);
  process.env.TETHERD = daemon;
  for (const name of ["TETHER_SOCKET", "TETHER_DIR", "TETHER_POLICY"]) delete process.env[name];

  where = fs.mkdtempSync(path.join(os.tmpdir(), "tsdk"));
  fs.writeFileSync(path.join(where, "script.json"), JSON.stringify(SCRIPT));
  fs.writeFileSync(path.join(where, "deny.toml"), policy("deny"));
  fs.writeFileSync(path.join(where, "ask.toml"), policy("ask"));
});

afterEach(() => {
  for (const name of VARIABLES) {
    if (saved[name] === undefined) delete process.env[name];
    else process.env[name] = saved[name];
  }
  // Stop the daemon this test started rather than wait out its idle time.
  spawnSync("pkill", ["-f", path.join(where, "runs", "tetherd.sock")]);
  fs.rmSync(where, { recursive: true, force: true });
});

function options(extra: tether.RunOptions = {}): tether.RunOptions {
  return {
    policy: path.join(where, "deny.toml"),
    provider: "fake",
    script: path.join(where, "script.json"),
    dir: path.join(where, "runs"),
    ...extra,
  };
}

function trace(run: tether.Run): any[] {
  return fs
    .readFileSync(run.tracePath, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

async function refused(doing: Promise<unknown>): Promise<tether.TetherError> {
  try {
    await doing;
  } catch (error) {
    assert.ok(error instanceof tether.TetherError, `not a TetherError: ${error}`);
    return error;
  }
  assert.fail("the call was not refused");
}

test("an agent loop runs to the end", async () => {
  let said: string | undefined;
  const run = await tether.withRun(options(), async (run) => {
    assert.deepEqual(
      run.tools.map((tool) => tool.name),
      ["exec", "read_file", "write_file", "list_dir"],
    );
    assert.deepEqual((run.tools[0]?.inputSchema as { required: string[] }).required, ["argv"]);

    const messages = [tether.system("Be brief."), tether.user("Tidy up.")];
    for (;;) {
      const reply = await run.complete({ model: "fake-1", messages });
      messages.push(reply.message);
      if (reply.toolCalls.length === 0) {
        said = reply.text;
        break;
      }
      const results = [];
      for (const call of reply.toolCalls) results.push(await run.callTool(call));
      messages.push(tether.toolResults(results));
    }
    await run.finish(said);
    return run;
  });

  assert.equal(said, "Done.");
  const events = trace(run);
  assert.equal(events[0].type, "run.created");
  assert.equal(events.at(-1).type, "run.finished");
  assert.equal(events.at(-1).data.outcome, "Done.");

  // The write really happened, in the run's own workspace.
  const workspace = run.tracePath.replace(/\.jsonl$/, ".workspace");
  assert.equal(fs.readFileSync(path.join(workspace, "notes.txt"), "utf8"), "from typescript\n");

  // The denied `rm` came back as a failed result, not an exception.
  const results = events.filter((e) => e.type === "tool.result").map((e) => e.data);
  assert.deepEqual(
    results.map((r) => r.is_error),
    [false, true],
  );
  const rules = events
    .filter((e) => e.type === "policy.decision" && e.data.rule)
    .map((e) => e.data.rule);
  assert.deepEqual(rules, ["writes", "no-rm"]);
});

test("a reply and a tool result carry what the daemon sent", async () => {
  await tether.withRun(options(), async (run) => {
    const reply = await run.complete({ model: "fake-1", messages: [tether.user("Tidy up.")] });
    assert.equal(reply.message.role, "assistant");
    assert.equal(reply.text, "");
    assert.equal(reply.stopReason, null);
    assert.equal(reply.inputTokens, 0);
    assert.deepEqual(reply.toolCalls, [
      {
        id: "call_1",
        name: "write_file",
        arguments: { path: "notes.txt", content: "from typescript\n" },
      },
    ]);

    const result = await run.callTool(reply.toolCalls[0]!);
    assert.deepEqual(
      { id: result.id, isError: result.isError, content: result.content },
      { id: "call_1", isError: false, content: { bytes: 16 } },
    );
    assert.equal(typeof result.durationMs, "number");
    assert.deepEqual(tether.toolResults([result]), {
      role: "tool",
      content: [{ type: "tool_result", id: "call_1", content: { bytes: 16 }, is_error: false }],
    });
  });
});

test("withRun finishes the run when the body returns and halts it when the body throws", async () => {
  const finished = await tether.withRun(options(), async (run) => {
    await run.complete({ model: "fake-1", messages: [tether.user("Tidy up.")] });
    return run;
  });
  assert.equal(trace(finished).at(-1).type, "run.finished");

  let halted: tether.Run | undefined;
  await assert.rejects(
    tether.withRun(options(), async (run) => {
      halted = run;
      await run.complete({ model: "fake-1", messages: [tether.user("Tidy up.")] });
      throw new RangeError("the agent broke");
    }),
    RangeError,
  );
  const last = trace(halted!).at(-1);
  assert.deepEqual([last.type, last.data.state], ["run.state", "halted"]);
  assert.equal(last.data.reason, "kill switch: halt requested");
});

test("a halted run throws Halted", async () => {
  const run = await tether.Run.start(options());
  await run.halt();
  const error = await refused(
    run.complete({ model: "fake-1", messages: [tether.user("More.")] }),
  );
  assert.ok(error instanceof tether.Halted);
  assert.equal(error.code, status.FAILED_PRECONDITION);
  const again = await refused(run.callTool({ id: "x", name: "exec", arguments: { argv: ["ls"] } }));
  assert.ok(again instanceof tether.Halted);
  run.close();
});

test("an ask is answered while the call that asked is waiting", async () => {
  await tether.withRun(options({ policy: path.join(where, "ask.toml") }), async (run) => {
    await run.complete({ model: "fake-1", messages: [tether.user("Tidy up.")] });
    // Not awaited yet: it waits for the answer.
    const waiting = run.callTool({ id: "c", name: "exec", arguments: { argv: ["rm", "-rf", "."] } });
    // The events stream shows the ask; answer it.
    for await (const event of run.events()) {
      if (event.type === "policy.ask") {
        assert.match(event.data.data.prompt, /rm/);
        await run.answer(event.data.data.subject, false, "node");
        break;
      }
    }
    const result = await waiting;
    assert.equal(result.isError, true);
    assert.equal(result.content, "denied by node");
  });
});

test("events streams the whole trace of a finished run", async () => {
  await tether.withRun(options(), async (run) => {
    await run.complete({ model: "fake-1", messages: [tether.user("Tidy up.")] });
    await run.finish("early");
    const seen = [];
    for await (const event of run.events()) seen.push(event);
    assert.deepEqual(
      seen.map((event) => event.seq),
      seen.map((_, at) => at),
    );
    assert.deepEqual(
      seen.map((event) => event.data),
      trace(run),
    );
  });
});

test("refused requests throw with the status", async () => {
  const missing = await refused(
    tether.Run.start(options({ policy: path.join(where, "missing.toml") })),
  );
  assert.equal(missing.code, status.INVALID_ARGUMENT);
  assert.ok(!(missing instanceof tether.Halted));

  const provider = await refused(tether.Run.start(options({ provider: "llama" })));
  assert.equal(provider.code, status.UNIMPLEMENTED);

  await tether.withRun(options(), async (run) => {
    const tool = await refused(
      run.complete({ model: "fake-1", messages: [tether.user("hi")], tools: ["teleport"] }),
    );
    assert.equal(tool.code, status.INVALID_ARGUMENT);
  });
});

test("the environment fills in what was not passed", async () => {
  // What `tether run -- <command>` sets for the agent it starts.
  process.env.TETHER_DIR = path.join(where, "runs");
  process.env.TETHER_POLICY = path.join(where, "deny.toml");
  const run = await tether.withRun(
    { provider: "fake", script: path.join(where, "script.json") },
    async (run) => run,
  );
  // The temporary directory is behind a symlink on some systems.
  assert.equal(
    fs.realpathSync(path.dirname(run.tracePath)),
    fs.realpathSync(path.join(where, "runs")),
  );
  assert.equal(trace(run)[0].data.policy, policy("deny"));
});

test("no daemon and no binary is a clear error", async () => {
  delete process.env.TETHERD;
  process.env.PATH = "/nonexistent";
  const error = await refused(tether.Run.start(options()));
  assert.match(error.message, /neither tetherd nor tether is on PATH/);
});
