"""The client: one class, one gRPC request per method."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator, Optional, Sequence, Union

import grpc

from ._proto import tether_pb2 as pb
from ._proto import tether_pb2_grpc as pb_grpc

DEFAULT_DIR = ".tether"

# How long to wait for a daemon this client started to answer on its socket.
_DAEMON_START_SECS = 10.0


class TetherError(RuntimeError):
    """A call the daemon refused. ``code`` is the gRPC status code."""

    def __init__(self, code: grpc.StatusCode, message: str) -> None:
        super().__init__(message)
        self.code = code


class Halted(TetherError):
    """The run is halted or finished and takes no more calls.

    Tether halts a run when a budget is spent, a policy ask goes unanswered,
    the kill switch is tripped or the model fails. The message says which.
    """


@dataclass(frozen=True)
class Tool:
    """A tool the run offers, as listed when the run started."""

    name: str
    server: str
    description: str
    input_schema: Any


@dataclass(frozen=True)
class ToolCall:
    """A tool the model asked for."""

    id: str
    name: str
    arguments: Any


@dataclass(frozen=True)
class ToolResult:
    """What a tool call came to.

    A call that policy denied comes back here with ``is_error`` set, like any
    other failed call. It does not raise.
    """

    id: str
    content: Any
    is_error: bool
    duration_ms: int

    def block(self) -> dict:
        """This result as a content block for the next request's history."""
        return {
            "type": "tool_result",
            "id": self.id,
            "content": self.content,
            "is_error": self.is_error,
        }


@dataclass(frozen=True)
class Reply:
    """One model turn.

    Append ``message`` to your history exactly as it is. It can hold blocks
    only the provider understands, which must go back unchanged. Look at
    ``stop_reason`` before acting: ``refusal`` and ``max_tokens`` are replies
    that are not finished answers.
    """

    message: dict
    tool_calls: list
    stop_reason: Optional[str]
    input_tokens: int
    output_tokens: int
    cache_read_tokens: int
    cache_write_tokens: int

    @property
    def text(self) -> str:
        """The reply's text blocks, joined."""
        blocks = self.message.get("content", [])
        return "".join(b.get("text", "") for b in blocks if b.get("type") == "text")


@dataclass(frozen=True)
class Event:
    """One event of the trace."""

    seq: int
    type: str
    data: dict


def user(text: str) -> dict:
    """A user message holding ``text``."""
    return {"role": "user", "content": [{"type": "text", "text": text}]}


def system(text: str) -> dict:
    """A system message holding ``text``. Put it first to make it the system prompt."""
    return {"role": "system", "content": [{"type": "text", "text": text}]}


def tool_results(results: Sequence[ToolResult]) -> dict:
    """The message that hands a turn's tool results back to the model."""
    return {"role": "tool", "content": [result.block() for result in results]}


def _raise(error: grpc.RpcError) -> None:
    code, message = error.code(), error.details() or ""
    if code == grpc.StatusCode.FAILED_PRECONDITION:
        raise Halted(code, message) from None
    raise TetherError(code, message) from None


def _daemon_command() -> list:
    """The command that starts the daemon: ``tetherd``, or ``tether daemon``."""
    override = os.environ.get("TETHERD")
    if override:
        return [override]
    if shutil.which("tetherd"):
        return ["tetherd"]
    if shutil.which("tether"):
        return ["tether", "daemon"]
    raise TetherError(
        grpc.StatusCode.UNAVAILABLE,
        "no daemon is running and neither tetherd nor tether is on PATH; "
        "set TETHERD to the tetherd binary",
    )


def _connect(socket: Path) -> grpc.Channel:
    """A channel to the daemon on ``socket``, starting the daemon if needed."""

    def ready(channel: grpc.Channel, seconds: float) -> bool:
        try:
            grpc.channel_ready_future(channel).result(timeout=seconds)
            return True
        except grpc.FutureTimeoutError:
            return False

    target = f"unix:{socket}"
    # Over a Unix socket gRPC would send the socket's path as the HTTP/2
    # authority, which the daemon's HTTP stack rejects. Name one instead.
    options = [("grpc.default_authority", "localhost")]
    channel = grpc.insecure_channel(target, options)
    if socket.exists() and ready(channel, 1.0):
        return channel
    channel.close()

    socket.parent.mkdir(parents=True, exist_ok=True)
    with open(socket.parent / "tetherd.log", "ab") as log:
        subprocess.Popen(
            _daemon_command() + ["--socket", str(socket)],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=log,
            # Its own session, so it outlives this process and its Ctrl-C.
            start_new_session=True,
        )
    deadline = time.monotonic() + _DAEMON_START_SECS
    while time.monotonic() < deadline:
        if socket.exists():
            channel = grpc.insecure_channel(target, options)
            if ready(channel, 1.0):
                return channel
            channel.close()
        time.sleep(0.05)
    raise TetherError(
        grpc.StatusCode.UNAVAILABLE,
        f"the daemon did not come up on {socket}; see {socket.parent / 'tetherd.log'}",
    )


class Run:
    """A run under Tether.

    Creating one starts the run. Use it as a context manager: leaving the
    block finishes the run, or halts it if the block raised.

    ``policy`` is the path of a ``tether.toml``; without one there are no
    rules and every tool call asks. ``provider`` is ``"anthropic"`` or, for
    tests and demos, ``"fake"`` with ``script`` naming a file of scripted
    replies. ``dir`` is where the trace goes. Unset arguments fall back to
    ``TETHER_POLICY``, ``TETHER_DIR`` and ``TETHER_SOCKET``, which
    ``tether run -- <command>`` sets.
    """

    def __init__(
        self,
        *,
        policy: Union[str, os.PathLike, None] = None,
        provider: str = "anthropic",
        script: Union[str, os.PathLike, None] = None,
        dir: Union[str, os.PathLike, None] = None,
        workspace: Union[str, os.PathLike, None] = None,
        agent: str = "main",
        betas: Sequence[str] = (),
        socket: Union[str, os.PathLike, None] = None,
    ) -> None:
        # The daemon may have another working directory: send absolute paths.
        def absolute(path: Union[str, os.PathLike, None]) -> str:
            return str(Path(path).resolve()) if path else ""

        run_dir = Path(dir or os.environ.get("TETHER_DIR") or DEFAULT_DIR).resolve()
        socket_path = Path(
            socket or os.environ.get("TETHER_SOCKET") or run_dir / "tetherd.sock"
        ).resolve()
        self._channel = _connect(socket_path)
        self._stub = pb_grpc.TetherStub(self._channel)
        self._over = False
        try:
            created = self._stub.CreateRun(
                pb.CreateRunRequest(
                    agent=agent,
                    policy_path=absolute(policy or os.environ.get("TETHER_POLICY")),
                    workspace=absolute(workspace),
                    provider=provider,
                    script_path=absolute(script),
                    dir=str(run_dir),
                    betas=list(betas),
                )
            )
        except grpc.RpcError as error:
            self._channel.close()
            _raise(error)
        self.id: str = created.run_id
        self.trace_path: str = created.trace_path
        self.tools: list = [
            Tool(t.name, t.server, t.description, json.loads(t.input_schema_json))
            for t in created.tools
        ]

    def complete(
        self,
        *,
        model: str,
        messages: Sequence[dict],
        tools: Optional[Sequence[str]] = None,
        params: Optional[dict] = None,
    ) -> Reply:
        """Ask the model for its next turn.

        ``messages`` is the whole history. ``tools`` names the tools to
        offer; by default all of the run's. ``params`` is passed to the
        provider as the request's other fields (``max_tokens``,
        ``output_config`` and so on).
        """
        names = [tool.name for tool in self.tools] if tools is None else list(tools)
        try:
            response = self._stub.Complete(
                pb.ModelRequest(
                    run_id=self.id,
                    model=model,
                    messages_json=json.dumps(list(messages)),
                    tools=names,
                    params_json=json.dumps(params) if params else "",
                )
            )
        except grpc.RpcError as error:
            self._over = error.code() == grpc.StatusCode.FAILED_PRECONDITION
            _raise(error)
        return Reply(
            message=json.loads(response.message_json),
            tool_calls=[
                ToolCall(c.id, c.name, json.loads(c.arguments_json))
                for c in response.tool_calls
            ],
            stop_reason=response.stop_reason or None,
            input_tokens=response.input_tokens,
            output_tokens=response.output_tokens,
            cache_read_tokens=response.cache_read_tokens,
            cache_write_tokens=response.cache_write_tokens,
        )

    def call_tool(self, call: ToolCall) -> ToolResult:
        """Run a tool the model asked for, through policy and the sandbox."""
        try:
            result = self._stub.CallTool(
                pb.ToolCallRequest(
                    run_id=self.id,
                    call=pb.ToolCall(
                        id=call.id,
                        name=call.name,
                        arguments_json=json.dumps(call.arguments),
                    ),
                )
            )
        except grpc.RpcError as error:
            self._over = error.code() == grpc.StatusCode.FAILED_PRECONDITION
            _raise(error)
        return ToolResult(
            id=result.id,
            content=json.loads(result.content_json),
            is_error=result.is_error,
            duration_ms=result.duration_ms,
        )

    def finish(self, outcome: Any = None) -> None:
        """Say the agent is done. ``outcome`` is recorded in the trace."""
        try:
            self._stub.Finish(
                pb.FinishRequest(run_id=self.id, outcome_json=json.dumps(outcome))
            )
        except grpc.RpcError as error:
            _raise(error)
        finally:
            self._over = True

    def halt(self) -> None:
        """Trip the run's kill switch."""
        try:
            self._stub.Halt(pb.RunRef(run_id=self.id))
        except grpc.RpcError as error:
            _raise(error)
        finally:
            self._over = True

    def answer(self, subject_seq: int, allow: bool, answered_by: str = "sdk") -> None:
        """Answer a policy ask. The call that asked is waiting in another thread."""
        try:
            self._stub.Answer(
                pb.AskAnswer(
                    run_id=self.id,
                    subject_seq=subject_seq,
                    allow=allow,
                    answered_by=answered_by,
                )
            )
        except grpc.RpcError as error:
            _raise(error)

    def events(self) -> Iterator[Event]:
        """The trace so far, then each new event until the run ends."""
        try:
            for event in self._stub.Events(pb.RunRef(run_id=self.id)):
                yield Event(event.seq, event.type, json.loads(event.json))
        except grpc.RpcError as error:
            _raise(error)

    def close(self) -> None:
        """Close the connection. The run itself is not affected."""
        self._channel.close()

    def __enter__(self) -> "Run":
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        try:
            if not self._over:
                # Left without a word: done if the block ended, stopped if it raised.
                if exc_type is None:
                    self.finish()
                else:
                    self.halt()
        finally:
            self.close()
