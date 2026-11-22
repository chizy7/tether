#!/usr/bin/env python3
"""Minimal MCP server over stdio for tests. No dependencies.

Tools: `echo` returns its `text` argument; `fail` returns an error result.
Protocol version is taken from the client's initialize request.
"""
import json
import sys

import os
import time

TOOLS = [
    {"name": "echo", "description": "Echo text back",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "fail", "description": "Always returns an error result",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "slow", "description": "Sleeps for `secs` before answering",
     "inputSchema": {"type": "object", "properties": {"secs": {"type": "number"}}}},
    {"name": "env", "description": "Returns the value of environment variable `name`, or null",
     "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
    # Same name as a built-in sandbox tool, to exercise collision prefixing.
    {"name": "read_file", "description": "Not the sandbox one",
     "inputSchema": {"type": "object", "properties": {}}},
]


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        forced = sys.argv[sys.argv.index("--protocol") + 1] if "--protocol" in sys.argv else None
        version = forced or req["params"].get("protocolVersion", "2025-06-18")
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake-mcp", "version": "0.0.1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": rid, "result": {}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": rid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        name = req["params"]["name"]
        args = req["params"].get("arguments") or {}
        if name == "echo":
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": args.get("text", "")}], "isError": False}})
        elif name == "slow":
            time.sleep(float(args.get("secs", 0)))
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": "done"}], "isError": False}})
        elif name == "env":
            value = os.environ.get(args["name"])
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": json.dumps(value)}], "isError": False}})
        elif name == "read_file":
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": "external read_file"}], "isError": False}})
        elif name == "fail":
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": "fail tool failed"}], "isError": True}})
        else:
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32602, "message": f"unknown tool {name}"}})
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": f"unknown method {method}"}})
