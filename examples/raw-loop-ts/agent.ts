// A raw agent loop in TypeScript, wrapped by Tether.
//
// The TypeScript twin of examples/raw-loop. The agent is `agent` below: ask
// the model, run the tools it asks for, feed the results back, stop when it
// asks for none. Its two call sites go through a Tether `Run` instead of a
// provider SDK and a tool dispatcher, and that is the whole integration.
//
// By default the model is scripted, so this needs no API key. `--model
// anthropic` swaps in a real model and nothing else changes: the tools are
// real either way and run in the sandbox.
//
//     npm start
//     ANTHROPIC_API_KEY=... npm start -- --model anthropic
//     OPENAI_BASE_URL=http://localhost:11434/v1 npm start -- --model openai_compat <model name>

import * as path from "node:path";

import { Halted, Run, type RunOptions, system, toolResults, user, withRun } from "tether-agents";

const HERE = __dirname;
const SYSTEM =
  "You are working in a sandboxed workspace directory. " +
  "Use the tools to do what is asked, then say what you did in one sentence.";
const TASK = "Write hello.txt, then show me what is in it.";

/** The agent. Everything Tether-specific is the two `run.` calls. */
async function agent(run: Run, model: string, params: Record<string, unknown>): Promise<string> {
  const messages = [system(SYSTEM), user(TASK)];
  for (;;) {
    const reply = await run.complete({ model, messages, params });
    // The reply goes back into the history exactly as it came.
    messages.push(reply.message);
    const results = [];
    for (const call of reply.toolCalls) results.push(await run.callTool(call));
    // A refusal or a cut-off answer is a reply, but not a finished one. The
    // last two are what an OpenAI-style server calls the same things.
    if (["refusal", "max_tokens", "content_filter", "length"].includes(reply.stopReason ?? "")) {
      return `stopped early (${reply.stopReason}): ${reply.text}`;
    }
    if (results.length === 0) return reply.text;
    messages.push(toolResults(results));
  }
}

async function main(): Promise<number> {
  const args = process.argv.slice(2);
  let settings: RunOptions;
  let model: string;
  let params: Record<string, unknown> = {};
  if (args.length === 0 || args.join(" ") === "--model fake") {
    // What the scripted model does: write a file, read it back, report.
    settings = { provider: "fake", script: path.join(HERE, "script.json") };
    model = "fake-1";
  } else if (args.join(" ") === "--model anthropic") {
    // If the model declines a request, let the API retry it on another
    // model in the same call. That is a beta feature and needs its header.
    settings = { provider: "anthropic", betas: ["server-side-fallback-2026-07-01"] };
    // A small task: ask for the least reasoning effort.
    model = "claude-opus-5-5";
    params = { output_config: { effort: "low" }, fallbacks: "default" };
  } else if (args.length === 3 && args[0] === "--model" && args[1] === "openai_compat") {
    // Any server with an OpenAI-style chat completions endpoint, at
    // OPENAI_BASE_URL. The model is whatever that server calls it.
    settings = { provider: "openai_compat" };
    model = args[2] as string;
  } else {
    console.error("usage: npm start -- [--model fake|anthropic|openai_compat <model name>]");
    return 2;
  }

  // The file tools and a few read-only commands are allowed; anything else
  // asks, and with nobody answering an ask halts the run.
  const where = { policy: path.join(HERE, "tether.toml"), dir: path.join(HERE, ".tether") };
  try {
    const answer = await withRun({ ...where, ...settings }, async (run) => {
      console.log(`trace: ${run.tracePath}`);
      const answer = await agent(run, model, params);
      await run.finish(answer);
      return answer;
    });
    console.log(answer);
    return 0;
  } catch (error) {
    if (error instanceof Halted) {
      console.error(`halted: ${error.message}`);
      return 1;
    }
    throw error;
  }
}

main().then((code) => {
  process.exitCode = code;
});
