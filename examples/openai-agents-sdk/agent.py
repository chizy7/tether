"""An OpenAI Agents SDK agent, wrapped by Tether.

The loop here is the framework's: `Runner.run` decides when to call the model
and when to run a tool. Tether takes its two call sites. `TetherModel` is the
agent's model, so every model call goes through `run.complete`, and
`tether_tools` are its tools, so every tool call goes through `run.call_tool`.
Policy, the sandbox, budgets and the trace all follow from those two.

By default the model is scripted, so this needs no API key of any kind.

    python agent.py
    ANTHROPIC_API_KEY=... python agent.py --model anthropic
"""

import asyncio
import json
import sys
from pathlib import Path

from agents import Agent, FunctionTool, Model, ModelResponse, Runner, Usage, set_tracing_disabled
from openai.types.responses import ResponseFunctionToolCall, ResponseOutputMessage, ResponseOutputText

import tether

HERE = Path(__file__).resolve().parent
SYSTEM = (
    "You are working in a sandboxed workspace directory. "
    "Use the tools to do what is asked, then say what you did in one sentence."
)
TASK = "Write hello.txt, then show me what is in it."


class TetherModel(Model):
    """The framework's model: each turn is one `run.complete`."""

    def __init__(self, run: tether.Run, model: str, params: dict, results: dict):
        self.run, self.model, self.params, self.results = run, model, params, results
        self.messages, self.seen = [], 0  # the history, in Tether's form

    async def get_response(self, system_instructions, input, model_settings, tools, *_, **__):
        if not self.messages and system_instructions:
            self.messages.append(tether.system(system_instructions))
        items = [{"role": "user", "content": input}] if isinstance(input, str) else input
        # Only what is new: replies are already here, exactly as they came.
        new, self.seen = items[self.seen:], len(items)
        done = [self.results[i["call_id"]] for i in new if i.get("type") == "function_call_output"]
        if done:
            self.messages.append(tether.tool_results(done))
        self.messages += [tether.user(i["content"]) for i in new if i.get("role") == "user"]
        reply = await asyncio.to_thread(
            self.run.complete, model=self.model, messages=self.messages,
            tools=[tool.name for tool in tools], params=self.params)
        self.messages.append(reply.message)
        output = [ResponseFunctionToolCall(call_id=call.id, name=call.name, type="function_call",
                                           arguments=json.dumps(call.arguments)) for call in reply.tool_calls]
        if reply.text:
            text = ResponseOutputText(text=reply.text, type="output_text", annotations=[])
            output.insert(0, ResponseOutputMessage(id="msg", content=[text], role="assistant",
                                                   status="completed", type="message"))
        usage = Usage(requests=1, input_tokens=reply.input_tokens, output_tokens=reply.output_tokens,
                      total_tokens=reply.input_tokens + reply.output_tokens)
        return ModelResponse(output=output, usage=usage, response_id=None)

    def stream_response(self, *_, **__):
        raise NotImplementedError("Tether returns each model turn whole")


def tether_tools(run: tether.Run, results: dict) -> list:
    """The run's tools as the framework's: each call is one `run.call_tool`."""
    def wrap(spec: tether.Tool) -> FunctionTool:
        async def invoke(context, arguments: str) -> str:
            call = tether.ToolCall(context.tool_call_id, spec.name, json.loads(arguments or "{}"))
            result = results[call.id] = await asyncio.to_thread(run.call_tool, call)
            return result.content if isinstance(result.content, str) else json.dumps(result.content)
        return FunctionTool(name=spec.name, description=spec.description, on_invoke_tool=invoke,
                            params_json_schema=spec.input_schema, strict_json_schema=False)
    return [wrap(spec) for spec in run.tools]


def main() -> int:
    args = sys.argv[1:]
    if args in ([], ["--model", "fake"]):
        settings = dict(provider="fake", script=HERE / "script.json")
        model, params = "fake-1", {}
    elif args == ["--model", "anthropic"]:
        settings = dict(provider="anthropic")
        model, params = "claude-opus-5-5", {"output_config": {"effort": "low"}}
    else:
        print("usage: agent.py [--model fake|anthropic]", file=sys.stderr)
        return 2

    # The framework's own tracing uploads to OpenAI. Tether keeps the trace.
    set_tracing_disabled(True)
    with tether.Run(policy=HERE / "tether.toml", dir=HERE / ".tether", **settings) as run:
        print(f"trace: {run.trace_path}")
        results: dict = {}
        agent = Agent(name="raw-loop", instructions=SYSTEM, tools=tether_tools(run, results),
                      model=TetherModel(run, model, params, results))
        try:
            answer = Runner.run_sync(agent, TASK).final_output
        except Exception as error:
            # A halt inside a tool reaches here wrapped in the framework's own error.
            halted = error if isinstance(error, tether.Halted) else error.__cause__ or error.__context__
            if not isinstance(halted, tether.Halted):
                raise
            print(f"halted: {halted}", file=sys.stderr)
            return 1
        run.finish(answer)
    print(answer)
    return 0


if __name__ == "__main__":
    sys.exit(main())
