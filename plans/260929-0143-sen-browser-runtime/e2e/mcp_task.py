#!/usr/bin/env python3
"""Run one browser tool through the agent's real surface: `senclaw core-server`
over stdio (MCP), hosting only senclaw-browser on engine v2, which calls the
isolated daemon's /api/browser-agent/*.

usage: mcp_task.py <senclaw-bin> <scratch-home> <api-url> <tool> <json-args>
Prints the tool's result text (JSON) on stdout.
"""
import json
import os
import subprocess
import sys

binary, home, api, tool, args = sys.argv[1:6]
env = {
    "HOME": home,
    "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
    "SENCLAW_CORE_SERVERS": "senclaw-browser",
    "SENCLAW_BROWSER_ENGINE": "v2",
    "SENCLAW_BROWSER_API_URL": api,
    "SENCLAW_AGENT_ID": "e2e-chat",
    "SENCLAW_WS_PORT": "28789",
    "RUST_LOG": "warn",
}
proc = subprocess.Popen([binary, "core-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=sys.stderr, env=env)


def send(msg):
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()


def answer(want):
    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        if msg.get("id") == want:
            if "error" in msg:
                raise SystemExit(f"MCP error: {msg['error']}")
            return msg["result"]
    raise SystemExit("core-server closed the connection")


try:
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                     "clientInfo": {"name": "senclaw-e2e", "version": "0"}}})
    answer(1)
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    names = sorted(t["name"] for t in answer(2)["tools"])
    expected = {"browser_task", "browser_approve", "browser_look", "browser_do", "browser_open", "browser_read"}
    missing = expected - set(names)
    if missing:
        raise SystemExit(f"engine v2 tools missing from core-server: {sorted(missing)} (has {names})")
    if "browser_navigate" in names:
        raise SystemExit("the legacy browser tools are still registered next to engine v2")
    send({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
          "params": {"name": tool, "arguments": json.loads(args)}})
    result = answer(3)
    texts = [c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"]
    print(texts[0] if texts else json.dumps(result))
finally:
    proc.stdin.close()
    proc.terminate()
