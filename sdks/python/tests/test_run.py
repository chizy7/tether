"""The SDK against a real daemon, with the scripted model. No API key."""

import json
import os
import shutil
import subprocess
import tempfile
import threading
from pathlib import Path

import grpc
import pytest

import tether

REPO = Path(__file__).resolve().parents[3]

SCRIPT = {
    "replies": [
        [{"type": "tool_use", "id": "call_1", "name": "write_file",
          "arguments": {"path": "notes.txt", "content": "from python\n"}}],
        [{"type": "tool_use", "id": "call_2", "name": "exec",
          "arguments": {"argv": ["rm", "notes.txt"]}}],
        [{"type": "text", "text": "Done."}],
    ]
}

POLICY = """
[[rule]]
name = "writes"
match = { tool = "sandbox.write_file" }
action = "allow"

[[rule]]
name = "no-rm"
match = { tool = "sandbox.exec", argv0 = "rm" }
action = "%s"
"""


def tetherd() -> str:
    path = os.environ.get("TETHERD") or str(REPO / "target" / "debug" / "tetherd")
    if not Path(path).exists():
        pytest.fail(f"{path} is not built; run `cargo build -p tetherd` first")
    return path


@pytest.fixture
def where(monkeypatch):
    """A short-lived directory with a script and a policy, and the daemon
    binary named for the SDK to start. Short path: sockets have a length limit."""
    monkeypatch.setenv("TETHERD", tetherd())
    # Found now: one test empties PATH, and teardown runs before it is restored.
    pkill = shutil.which("pkill")
    for name in ("TETHER_SOCKET", "TETHER_DIR", "TETHER_POLICY"):
        monkeypatch.delenv(name, raising=False)
    with tempfile.TemporaryDirectory(prefix="tsdk") as tmp:
        tmp = Path(tmp)
        (tmp / "script.json").write_text(json.dumps(SCRIPT))
        (tmp / "deny.toml").write_text(POLICY % "deny")
        (tmp / "ask.toml").write_text(POLICY % "ask")
        yield tmp
        # Stop the daemon this test started rather than wait out its idle time.
        if pkill:
            subprocess.run([pkill, "-f", str(tmp / "runs" / "tetherd.sock")], check=False)


def start(where: Path, policy: str = "deny.toml", **kwargs) -> tether.Run:
    return tether.Run(
        policy=where / policy,
        provider="fake",
        script=where / "script.json",
        dir=where / "runs",
        **kwargs,
    )


def trace(run: tether.Run) -> list:
    return [json.loads(line) for line in Path(run.trace_path).read_text().splitlines()]


def test_an_agent_loop_runs_to_the_end(where):
    with start(where) as run:
        assert [tool.name for tool in run.tools] == ["exec", "read_file", "write_file", "list_dir"]
        assert run.tools[0].input_schema["required"] == ["argv"]

        messages = [tether.system("Be brief."), tether.user("Tidy up.")]
        said = None
        while said is None:
            reply = run.complete(model="fake-1", messages=messages)
            messages.append(reply.message)
            if not reply.tool_calls:
                said = reply.text
                break
            results = [run.call_tool(call) for call in reply.tool_calls]
            messages.append(tether.tool_results(results))
        run.finish(said)

    assert said == "Done."
    events = trace(run)
    assert events[0]["type"] == "run.created"
    assert events[-1]["type"] == "run.finished"
    assert events[-1]["data"]["outcome"] == "Done."

    # The write really happened, in the run's own workspace.
    workspace = Path(run.trace_path).with_suffix(".workspace")
    assert (workspace / "notes.txt").read_text() == "from python\n"

    # The denied `rm` came back as a failed result, not an exception.
    results = [e["data"] for e in events if e["type"] == "tool.result"]
    assert [r["is_error"] for r in results] == [False, True]
    decisions = [e["data"] for e in events if e["type"] == "policy.decision" and e["data"]["rule"]]
    assert [d["rule"] for d in decisions] == ["writes", "no-rm"]


def test_reply_and_tool_result_carry_what_the_daemon_sent(where):
    with start(where) as run:
        reply = run.complete(model="fake-1", messages=[tether.user("Tidy up.")])
        assert reply.message["role"] == "assistant"
        assert reply.text == ""
        assert reply.stop_reason is None
        (call,) = reply.tool_calls
        assert (call.id, call.name) == ("call_1", "write_file")
        assert call.arguments == {"path": "notes.txt", "content": "from python\n"}

        result = run.call_tool(call)
        assert (result.id, result.is_error, result.content) == ("call_1", False, {"bytes": 12})
        assert result.block() == {
            "type": "tool_result", "id": "call_1", "content": {"bytes": 12}, "is_error": False,
        }


def test_leaving_the_block_finishes_the_run_and_raising_halts_it(where):
    with start(where) as finished:
        finished.complete(model="fake-1", messages=[tether.user("Tidy up.")])
    assert trace(finished)[-1]["type"] == "run.finished"

    with pytest.raises(ValueError):
        with start(where) as halted:
            halted.complete(model="fake-1", messages=[tether.user("Tidy up.")])
            raise ValueError("the agent broke")
    last = trace(halted)[-1]
    assert (last["type"], last["data"]["state"]) == ("run.state", "halted")
    assert last["data"]["reason"] == "kill switch: halt requested"


def test_a_halted_run_raises_halted(where):
    run = start(where)
    run.halt()
    with pytest.raises(tether.Halted) as error:
        run.complete(model="fake-1", messages=[tether.user("More.")])
    assert error.value.code == grpc.StatusCode.FAILED_PRECONDITION
    with pytest.raises(tether.Halted):
        run.call_tool(tether.ToolCall("x", "exec", {"argv": ["ls"]}))
    run.close()


def test_an_ask_is_answered_from_another_thread(where):
    with start(where, policy="ask.toml") as run:
        run.complete(model="fake-1", messages=[tether.user("Tidy up.")])
        outcome = {}

        def call():
            outcome["result"] = run.call_tool(tether.ToolCall("c", "exec", {"argv": ["rm", "-rf", "."]}))

        waiting = threading.Thread(target=call)
        waiting.start()
        # The events stream shows the ask; answer it.
        for event in run.events():
            if event.type == "policy.ask":
                assert "rm" in event.data["data"]["prompt"]
                run.answer(event.data["data"]["subject"], allow=False, answered_by="pytest")
                break
        waiting.join(timeout=10)
        assert not waiting.is_alive()
        assert outcome["result"].is_error
        assert outcome["result"].content == "denied by pytest"


def test_events_streams_the_whole_trace_of_a_finished_run(where):
    with start(where) as run:
        run.complete(model="fake-1", messages=[tether.user("Tidy up.")])
        run.finish("early")
        seen = list(run.events())
    assert [event.seq for event in seen] == list(range(len(seen)))
    assert [event.data for event in seen] == trace(run)


def test_refused_requests_raise_with_the_status(where):
    with pytest.raises(tether.TetherError) as error:
        tether.Run(policy=where / "missing.toml", provider="fake", script=where / "script.json", dir=where / "runs")
    assert error.value.code == grpc.StatusCode.INVALID_ARGUMENT
    assert not isinstance(error.value, tether.Halted)

    with pytest.raises(tether.TetherError) as error:
        tether.Run(policy=where / "deny.toml", provider="llama", dir=where / "runs")
    assert error.value.code == grpc.StatusCode.UNIMPLEMENTED

    with start(where) as run:
        with pytest.raises(tether.TetherError) as error:
            run.complete(model="fake-1", messages=[tether.user("hi")], tools=["teleport"])
        assert error.value.code == grpc.StatusCode.INVALID_ARGUMENT


def test_environment_fills_in_what_was_not_passed(where, monkeypatch):
    # What `tether run -- <command>` sets for the agent it starts.
    monkeypatch.setenv("TETHER_DIR", str(where / "runs"))
    monkeypatch.setenv("TETHER_POLICY", str(where / "deny.toml"))
    with tether.Run(provider="fake", script=where / "script.json") as run:
        pass
    assert Path(run.trace_path).parent == (where / "runs").resolve()
    assert trace(run)[0]["data"]["policy"] == (where / "deny.toml").read_text()


def test_no_daemon_and_no_binary_is_a_clear_error(where, monkeypatch):
    monkeypatch.delenv("TETHERD")
    monkeypatch.setenv("PATH", "/nonexistent")
    with pytest.raises(tether.TetherError) as error:
        start(where)
    assert "neither tetherd nor tether is on PATH" in str(error.value)
