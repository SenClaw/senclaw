"""{{title_name}} — a SenClaw Space App in one file, standard library only.

What the daemon does with this, in order:

1. Reads ``senclaw-manifest.json``. ``requires.python`` is checked before this
   file is ever executed.
2. The daemon creates a virtualenv at ``.venv`` in this directory for **every**
   python-runner app, and puts it first on PATH — so ``python`` here is never
   the user's system interpreter. There is no ``requirements.txt`` yet, so the
   venv stays empty and no ``pip install`` runs; add one and its contents are
   installed into that venv. (A package you installed globally is therefore
   *not* importable here — declare it in ``requirements.txt``.)
3. ``runtime.mode: "session"`` — nothing starts at boot. The app starts when the
   user opens it or an agent calls one of the tools below, and stops 60 seconds
   after the last request.

The tools stay in every agent's roster while this is stopped: the tool list is
cached and the MCP URL points at the daemon's proxy, which starts the app before
forwarding the call.

Run it by hand during development:

    SENCLAW_SPACE_APP_ID={{id}} PORT={{port}} python3 main.py
"""

from __future__ import annotations

import json
import os
import signal
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
WEB = HERE / "web"
APP_ID = os.environ.get("SENCLAW_SPACE_APP_ID") or "{{id}}"
BASE = (os.environ.get("SENCLAW_BASE_URL") or "http://127.0.0.1:18788").rstrip("/")
# This app's access token, injected by the daemon. Sent on every call to it:
# under the default strict mode a tokenless call to an app's data routes is
# refused, and a token presented against another app's id is refused always.
TOKEN = os.environ.get("SENCLAW_TOKEN_ACCESS_APP", "")
API_VERSION = os.environ.get("SENCLAW_API_VERSION", "{{api_version}}")
# Loopback by default. A Space App authenticates nothing of its own — the daemon
# reaches it over 127.0.0.1 and the UI is same-origin — so binding 0.0.0.0 hands
# the whole REST + MCP surface to anyone on the LAN. Set SENCLAW_BIND_HOST to
# opt in to that explicitly.
HOST = os.environ.get("SENCLAW_BIND_HOST") or "127.0.0.1"
PORT = int(os.environ.get("PORT") or {{port}})
STARTED = time.time()


# ---------------------------------------------------------------------------
# Talking to the daemon
# ---------------------------------------------------------------------------


def daemon(method: str, suffix: str, body: dict | None = None, *, missing_ok: bool = False):
    """`missing_ok` only for routes where 404 genuinely means "not set".

    It is the config KV and nothing else. Treating 404 as None everywhere turns
    a bridge that has moved — an older daemon, a proxy path change, a typo in
    the app id — into an empty *successful* summary the agent cannot tell from a
    real one.
    """
    url = f"{BASE}/api/space/apps/{urllib.parse.quote(APP_ID)}{suffix}"
    headers = {"Accept": "application/json", "x-senclaw-api-version": API_VERSION}
    if TOKEN:
        headers["x-senclaw-app-token"] = TOKEN
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            return json.loads(resp.read().decode("utf-8", "replace") or "{}")
    except urllib.error.HTTPError as e:
        if e.code == 404 and missing_ok:
            return None
        detail = e.read().decode("utf-8", "replace")
        raise RuntimeError(f"{method} {suffix} → HTTP {e.code}: {detail}") from None


def llm(prompt: str, max_tokens: int = 600) -> str:
    """Ask the daemon's model. The app never holds a provider API key."""
    body = daemon("POST", "/bridge", {
        # The wire field is `action`, not `capability`. The daemon's request
        # struct requires it, and a body without it is rejected by the JSON
        # extractor with a 422 before any handler runs.
        "action": "llm.request",
        # Only these fields are honoured — temperature and friends are not part
        # of the bridge contract and are silently dropped.
        "payload": {"prompt": prompt, "maxTokens": max_tokens},
    }) or {}
    # A failed completion comes back as HTTP **200** with status "error".
    # Checking only the HTTP status turns a provider outage into a successful
    # empty summary, which the agent has no way to notice.
    if body.get("status") == "error":
        raise RuntimeError(body.get("message") or "model trả về lỗi không rõ")
    if body.get("finish") == "length":
        raise RuntimeError("câu trả lời bị cắt ở maxTokens — chia nhỏ công việc ra")
    return body.get("text") or body.get("content") or ""


def get_config(key: str, default=None):
    """The config KV, shared with the app's own settings UI."""
    payload = daemon("GET", f"/config/{urllib.parse.quote(key)}", missing_ok=True)
    return (payload or {}).get("value", default) if payload else default


def set_config(key: str, value):
    daemon("PUT", f"/config/{urllib.parse.quote(key)}", {"value": value})


# ---------------------------------------------------------------------------
# MCP: what agents can do with this app.
#
# The description is the only thing the model sees when choosing a tool — say
# what it does *and when to reach for it*. An error that reads like a sentence
# tells the agent what to do differently; a transport error tells it nothing.
# ---------------------------------------------------------------------------


def tool_status(_args: dict):
    return {
        "app": APP_ID,
        "python": sys.version.split()[0],
        # Non-empty when the daemon built a venv for this app — the tell that
        # dependency isolation actually happened.
        "venv": sys.prefix if sys.prefix != sys.base_prefix else None,
        "uptimeSecs": round(time.time() - STARTED, 1),
    }


def tool_summarise(args: dict):
    text = (args.get("text") or "").strip()
    if not text:
        return {"isError": True, "content": [
            {"type": "text", "text": "`text` đang rỗng — truyền đoạn văn bản cần tóm tắt."}
        ]}
    return llm(f"Tóm tắt đoạn sau thành đúng ba câu:\n\n{text}")


TOOLS = {
    "{{snake_name}}_status": {
        "description": "Xem {{title_name}} đang chạy ra sao: thời gian hoạt động và phiên bản Python. "
                       "Dùng khi người dùng hỏi app còn sống không.",
        "inputSchema": {"type": "object", "properties": {}},
        "run": tool_status,
    },
    "{{snake_name}}_summarise": {
        "description": "Tóm tắt một đoạn văn bản thành đúng ba câu. "
                       "Dùng khi người dùng đưa một đoạn dài và muốn ý chính.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string", "description": "Đoạn văn bản cần tóm tắt."}},
            "required": ["text"],
        },
        "run": tool_summarise,
    },
}


def handle_mcp(req: dict) -> dict:
    rid = req.get("id")
    ok = lambda result: {"jsonrpc": "2.0", "id": rid, "result": result}  # noqa: E731
    err = lambda code, msg: {"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": msg}}  # noqa: E731
    method = req.get("method")
    try:
        if method == "initialize":
            return ok({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "{{mcp_name}}", "version": "0.1.0"},
            })
        # SenClaw sends this as a request with an id, not a notification, and
        # ignores the reply — but erroring on it looks like a broken server.
        if method in ("ping", "initialized", "notifications/initialized"):
            return ok({})
        if method == "tools/list":
            return ok({"tools": [
                {"name": n, "description": t["description"], "inputSchema": t["inputSchema"]}
                for n, t in TOOLS.items()
            ]})
        if method == "tools/call":
            params = req.get("params") or {}
            name = params.get("name") or ""
            tool = TOOLS.get(name)
            if tool is None:
                return err(-32602, f"không có tool tên {name} (đang có: {', '.join(TOOLS)})")
            out = tool["run"](params.get("arguments") or {})
            if isinstance(out, dict) and "content" in out:
                return ok(out)
            text = out if isinstance(out, str) else json.dumps(out, ensure_ascii=False, indent=2)
            return ok({"content": [{"type": "text", "text": text}]})
        return err(-32601, f"method not found: {method}")
    except Exception as e:  # noqa: BLE001 — the agent needs the message, not a traceback
        return err(-32603, str(e))


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------

MIME = {".html": "text/html", ".js": "text/javascript", ".css": "text/css",
        ".json": "application/json", ".svg": "image/svg+xml", ".png": "image/png"}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass  # the daemon captures stdout; per-request noise buries real errors

    def _send(self, status: int, body, ctype: str = "application/json"):
        raw = body if isinstance(body, bytes) else json.dumps(body, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):  # noqa: N802
        path = urllib.parse.urlparse(self.path).path
        # runtime.healthPath. The daemon waits on this before it calls the app
        # started and polls it afterwards, so it must stay cheap and never block.
        if path == "/api/status":
            return self._send(200, {"ok": True, "app": APP_ID,
                                    "uptimeSecs": round(time.time() - STARTED, 1)})
        return self._serve_static(path)

    def do_POST(self):  # noqa: N802
        path = urllib.parse.urlparse(self.path).path
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""

        if path == "/api/mcp/sse":
            try:
                payload = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                return self._send(400, {"jsonrpc": "2.0", "id": None,
                                        "error": {"code": -32700, "message": "parse error"}})
            return self._send(200, handle_mcp(payload))

        if path == "/api/visit":
            try:
                visits = int(get_config("visits", 0) or 0) + 1
                set_config("visits", visits)
                return self._send(200, {"visits": visits})
            except Exception as e:  # noqa: BLE001
                return self._send(502, {"error": str(e)})

        return self._send(404, {"error": "not found", "path": path})

    def _serve_static(self, path: str):
        rel = "index.html" if path == "/" else path.lstrip("/")
        # Resolve first, then confirm the result is still inside the web root —
        # the check a hand-rolled static handler usually forgets, and the one
        # that stops `../../etc/passwd` from being served.
        #
        # `is_relative_to`, not a string prefix: a bare `startswith(str(WEB))`
        # also accepts a *sibling* whose name merely starts with "web"
        # (`web_dist/`, `web-build/`), which is exactly where an app author puts
        # files that are not meant to be public.
        root = WEB.resolve()
        target = (WEB / rel).resolve()
        if target != root and root not in target.parents:
            return self._send(403, {"error": "forbidden"})
        if not target.is_file():
            # Unknown paths are client-side routes in a single-page app.
            target = WEB / "index.html"
            if not target.is_file():
                return self._send(404, {"error": "not found", "path": path})
        return self._send(200, target.read_bytes(), MIME.get(target.suffix, "application/octet-stream"))


def main() -> None:
    server = ThreadingHTTPServer((HOST, PORT), Handler)

    # A session app is stopped when it goes idle: SIGTERM to the process group,
    # SIGKILL about two seconds later. Close and flush; do not start new work.
    def shutdown(signum, _frame):
        print(f"[{APP_ID}] signal {signum} — shutting down", flush=True)
        # From a thread, not inline. The handler runs on the main thread, which
        # is blocked inside serve_forever(), and shutdown() waits for an event
        # only serve_forever() can set — calling it here deadlocks, and the
        # daemon's ~2s grace period turns every idle stop into a SIGKILL.
        threading.Thread(target=server.shutdown, daemon=True).start()

    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, shutdown)

    print(f"[{APP_ID}] listening on http://{HOST}:{PORT}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
