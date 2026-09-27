#!/usr/bin/env python3
"""Smoke-test the aggregated `senclaw core-server` over real stdio MCP.

Everything so far is unit-tested in-process. This proves the actual claim:
ONE subprocess speaks MCP and publishes the tools of every built-in server
(wiki, workspace, memory, ...) that its env configures.

Nó bắt được một lỗi mà toàn bộ unit test bỏ sót: server tự giới thiệu là
"rmcp" vì `ServerInfo::new` điền serverInfo từ build env của thư viện. Chạy lại
sau khi đụng vào `core_server.rs` hoặc thêm một con mới.

    cargo build --bin senclaw
    mkdir -p /tmp/zk
    python3 scripts/core_mcp_smoke.py target/debug/senclaw /tmp/zk
"""
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


def send(proc, obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def read_reply(proc, want_id, timeout_note=""):
    """Read lines until a JSON-RPC response with the wanted id shows up.

    The server also emits notifications; anything that is not our response
    gets skipped rather than treated as a protocol error.
    """
    while True:
        line = proc.stdout.readline()
        if not line:
            raise SystemExit(f"server closed stdout before answering {want_id} {timeout_note}")
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue  # tracing output that escaped to stdout
        if msg.get("id") == want_id:
            return msg


def main():
    binary, scratch = sys.argv[1], Path(sys.argv[2])
    home = scratch / "home"
    wiki = home / "wiki"
    workspace = home / "workspace"
    for d in (wiki, workspace):
        d.mkdir(parents=True, exist_ok=True)

    env = dict(os.environ)
    env.update({
        # Only wiki + workspace are configured. Every other child must be
        # skipped without killing the process — that is the whole point of
        # `from_env() -> Option<Self>`.
        "SENCLAW_WIKI_DIR": str(wiki),
        "SENCLAW_WORKSPACE_STATE_FILE": str(home / "workspace-state.json"),
        "SENCLAW_DEFAULT_WORKSPACE": str(workspace),
        "SENCLAW_ALLOWED_WORK_DIRS": json.dumps([str(workspace)]),
        "HOME": str(home),
        "RUST_LOG": "error",
    })

    proc = subprocess.Popen(
        [binary, "core-server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=env, text=True, bufsize=1,
    )

    try:
        send(proc, {
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "core-smoke", "version": "0"},
            },
        })
        init = read_reply(proc, 1)
        server_name = init["result"]["serverInfo"]["name"]
        send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})

        send(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        tools = [t["name"] for t in read_reply(proc, 2)["result"]["tools"]]

        # Call one tool from each child through the single connection.
        send(proc, {
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "wiki_status", "arguments": {}},
        })
        wiki_out = read_reply(proc, 3)

        send(proc, {
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "workspace_info", "arguments": {}},
        })
        ws_out = read_reply(proc, 4)

        send(proc, {
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "core_status", "arguments": {}},
        })
        status_out = read_reply(proc, 5)
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    def text_of(reply):
        content = reply.get("result", {}).get("content", [])
        return "\n".join(c.get("text", "") for c in content)

    print(f"serverInfo.name : {server_name}")
    print(f"tools           : {len(tools)}")
    print(f"  wiki_*        : {sorted(t for t in tools if t.startswith('wiki_'))}")
    print(f"  workspace_*   : {sorted(t for t in tools if t.startswith('workspace_'))}")
    print(f"  core_*      : {sorted(t for t in tools if t.startswith('core_'))}")
    # js / litho / sandbox cần KHÔNG có biến môi trường bắt buộc nên luôn sống
    # — chúng có mặt ở đây là đúng, không phải rác.
    others = sorted(t for t in tools
                    if not t.startswith(("wiki_", "workspace_", "core_")))
    print(f"  con khác      : {len(others)} tool ({', '.join(others[:4])}…)")
    print()
    print("--- wiki_status ---");      print(text_of(wiki_out)[:300])
    print("--- workspace_info ---");   print(text_of(ws_out)[:300])
    print("--- core_status ---");    print(text_of(status_out)[:400])

    stderr = proc.stderr.read()
    if stderr.strip():
        print("\n--- stderr ---")
        print(stderr[:800])

    # Verdict
    ok = (
        server_name == "senclaw-core"
        and {"wiki_status", "wiki_tree", "wiki_read", "wiki_write"} <= set(tools)
        and {"workspace_info", "workspace_switch", "workspace_reset"} <= set(tools)
        and "core_status" in tools
        and "error" not in wiki_out
        and "error" not in ws_out
        and "error" not in status_out
    )
    print("\nKẾT LUẬN:", "ĐẠT" if ok else "KHÔNG ĐẠT")
    return 0 if ok else 1


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as _:
        sys.exit(main())
