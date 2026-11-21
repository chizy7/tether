"""Python client for Tether.

Your agent keeps its own loop. It calls :meth:`Run.complete` where it called
its provider SDK and :meth:`Run.call_tool` where it ran a tool, and Tether
does the rest: policy, sandbox, budgets and the trace.

    import tether

    with tether.Run(policy="tether.toml") as run:
        messages = [tether.user("Write hello.txt, then show me what is in it.")]
        while True:
            reply = run.complete(model="claude-opus-5-5", messages=messages)
            messages.append(reply.message)
            if not reply.tool_calls:
                run.finish(reply.text)
                break
            results = [run.call_tool(call) for call in reply.tool_calls]
            messages.append(tether.tool_results(results))

This module is a thin client for the daemon, ``tetherd``. Nothing here
decides anything: every call is one gRPC request.
"""

from ._run import (
    Event,
    Halted,
    Reply,
    Run,
    TetherError,
    Tool,
    ToolCall,
    ToolResult,
    system,
    tool_results,
    user,
)

__all__ = [
    "Event",
    "Halted",
    "Reply",
    "Run",
    "TetherError",
    "Tool",
    "ToolCall",
    "ToolResult",
    "system",
    "tool_results",
    "user",
]
