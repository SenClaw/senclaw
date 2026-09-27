"""Serving a Python Space App: health, static UI, REST, and MCP in one process.

The daemon expects one HTTP server per app, on the port it hands out in
``PORT``, answering:

* ``runtime.healthPath`` — anything 2xx. The daemon waits on this before it
  considers the app started, and the supervisor polls it.
* ``mcp.path`` — the app's MCP endpoint, JSON-RPC over HTTP POST.
* everything else — the app's own REST API and its UI, which the daemon
  reverse-proxies at ``/api/space/apps/<id>/proxy/…``.

This module is the whole of that, on ``http.server``: no framework, no
dependencies, no ``pip install`` step before the first launch.

The one thing worth reading before writing an app: **handle SIGTERM**. A
session app is stopped when it goes idle, and the daemon signals the process
group with SIGTERM and SIGKILLs it two seconds later. Two seconds is plenty to
flush, and nothing if you ignore the signal. :func:`serve` installs a handler
that closes the listener and runs your ``on_shutdown``.
"""

from __future__ import annotations

import hmac
import json
import mimetypes
import os
import signal
import threading
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable

from .client import HEADER_APP_TOKEN, app_token_from_env, bind_host, port as env_port

Handler = Callable[["Request"], Any]


class Request:
    """One incoming HTTP request, reduced to what an app handler needs."""

    def __init__(self, method: str, path: str, query: dict[str, list[str]], body: bytes,
                 headers: dict[str, str]) -> None:
        self.method = method
        self.path = path
        self.query = query
        self.body = body
        self.headers = headers

    def json(self) -> Any:
        """The request body as JSON, or ``None`` when there is no body."""
        if not self.body:
            return None
        return json.loads(self.body.decode("utf-8"))

    def param(self, name: str, default: str | None = None) -> str | None:
        values = self.query.get(name)
        return values[0] if values else default


class Response:
    """Return one of these from a handler when you need control over status or
    content type. Return a plain dict/list/str and it is JSON/text with 200."""

    def __init__(self, body: Any = b"", status: int = 200, content_type: str = "application/json",
                 headers: dict[str, str] | None = None) -> None:
        self.status = status
        self.content_type = content_type
        self.headers = headers or {}
        if isinstance(body, (dict, list)) or body is None:
            self.body = json.dumps(body).encode()
        elif isinstance(body, str):
            self.body = body.encode()
        else:
            self.body = body


def _authorized(req: "Request", path: str, token: str, skip: list[str]) -> bool:
    """Whether this request carries the app's access token, or is exempt."""
    for pattern in skip:
        if not pattern:
            continue
        if pattern.endswith("/*"):
            if path.startswith(pattern[:-1]):
                return True
        elif path == pattern:
            return True
    # Header names are case-insensitive on the wire; http.server hands them
    # back as sent, so compare folded rather than by exact key.
    presented = ""
    wanted = HEADER_APP_TOKEN.lower()
    for name, value in req.headers.items():
        if name.lower() == wanted:
            presented = value.strip()
            break
    if not presented:
        presented = (req.param("app_token") or "").strip()
    # Constant time: a plain `==` on a secret leaks the matched prefix length.
    return hmac.compare_digest(presented, token)


def serve(
    routes: dict[tuple[str, str], Handler] | None = None,
    *,
    health_path: str = "/health",
    static_dir: str | os.PathLike[str] | None = None,
    mcp_path: str | None = None,
    mcp_handler: Callable[[dict[str, Any]], dict[str, Any]] | None = None,
    on_shutdown: Callable[[], None] | None = None,
    default_port: int = 0,
    log: Callable[[str], None] = print,
    require_app_token: bool = False,
    auth_skip_paths: list[str] | None = None,
) -> None:
    """Run the app's HTTP server until it is stopped. Blocks.

    ``routes`` maps ``(method, path)`` to a handler; a path ending in ``/*``
    matches by prefix, and the handler gets the full path.

    ``require_app_token`` closes the app's own API to everything except the
    daemon. An app's REST and MCP endpoints have no authentication of their own:
    the port is open to every process on the machine. With this on, the only
    caller that gets through is the daemon — its proxy stamps the app's access
    token on every request it forwards (the UI iframe, the app's own fetches,
    MCP tool calls) — and a direct hit on the port answers 401.

    Off by default, because it breaks two real patterns that talk to the port
    directly: a browser extension dialling ``ws://127.0.0.1:<port>``, and a
    developer's curl. Both are served by ``auth_skip_paths`` (exact paths, or a
    trailing ``/*`` prefix). ``health_path`` is always exempt — the daemon's
    health check decides whether the app started, and it runs before anything is
    proxied. With no token in the environment the guard is inert, so a bare
    ``python app.py`` still works.
    """
    routes = dict(routes or {})
    listen_host = bind_host()
    listen_port = env_port(default_port)
    static_root = Path(static_dir).resolve() if static_dir else None
    guard_token = app_token_from_env() if require_app_token else ""
    skip_paths = [health_path, *(auth_skip_paths or [])]

    class App(BaseHTTPRequestHandler):
        # The daemon's log file is the app's log file. Per-request lines from
        # http.server would bury everything the app itself printed.
        def log_message(self, fmt: str, *args: Any) -> None:  # noqa: A003
            return

        def _send(self, resp: Response) -> None:
            self.send_response(resp.status)
            self.send_header("Content-Type", resp.content_type)
            self.send_header("Content-Length", str(len(resp.body)))
            for k, v in resp.headers.items():
                self.send_header(k, v)
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(resp.body)

        def _dispatch(self) -> None:
            from urllib.parse import parse_qs, urlsplit

            split = urlsplit(self.path)
            path = split.path
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""
            req = Request(
                self.command, path, parse_qs(split.query), body, dict(self.headers.items())
            )

            if guard_token and not _authorized(req, path, guard_token, skip_paths):
                return self._send(
                    Response(
                        {
                            "error": "this app only answers requests from the SenClaw daemon",
                            "code": "app_token_required",
                        },
                        status=401,
                    )
                )

            if mcp_path and path == mcp_path and mcp_handler:
                try:
                    payload = json.loads(body.decode("utf-8")) if body else {}
                except json.JSONDecodeError:
                    return self._send(
                        Response(
                            {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32700, "message": "parse error"}},
                            status=400,
                        )
                    )
                return self._send(Response(mcp_handler(payload)))

            handler = routes.get((self.command, path))
            if handler is None:
                for (method, pattern), h in routes.items():
                    if method == self.command and pattern.endswith("/*") and path.startswith(
                        pattern[:-1]
                    ):
                        handler = h
                        break
            if handler is not None:
                try:
                    out = handler(req)
                except Exception as e:  # an app bug must not take the server down
                    traceback.print_exc()
                    return self._send(Response({"error": str(e)}, status=500))
                return self._send(out if isinstance(out, Response) else Response(out))

            # A default health endpoint, *after* the routes — an app that
            # registers its own handler at `health_path` (to report uptime, a
            # database check, a version) must get to answer with it. Serving
            # `{"ok": true}` over the top of that is how a health endpoint ends
            # up claiming an app is fine when the app knows it is not.
            if path == health_path or (health_path == "/" and path == ""):
                return self._send(Response({"ok": True}))

            if static_root is not None:
                served = self._serve_static(path, static_root)
                if served is not None:
                    return self._send(served)

            self._send(Response({"error": "not found", "path": path}, status=404))

        @staticmethod
        def _serve_static(path: str, root: Path) -> Response | None:
            rel = path.lstrip("/") or "index.html"
            # Resolve, then confirm the result is still inside the root — the
            # check that stops `../../etc/passwd` from being served, and the one
            # a hand-rolled static handler is usually missing.
            target = (root / rel).resolve()
            if not str(target).startswith(str(root)):
                return Response({"error": "forbidden"}, status=403)
            if target.is_dir():
                target = target / "index.html"
            if not target.is_file():
                # A single-page app: unknown paths are routes, not missing files.
                index = root / "index.html"
                if not index.is_file():
                    return None
                target = index
            ctype = mimetypes.guess_type(str(target))[0] or "application/octet-stream"
            return Response(target.read_bytes(), content_type=ctype)

        do_GET = _dispatch
        do_HEAD = _dispatch
        do_POST = _dispatch
        do_PUT = _dispatch
        do_DELETE = _dispatch
        do_PATCH = _dispatch

    httpd = ThreadingHTTPServer((listen_host, listen_port), App)
    httpd.daemon_threads = True

    stopping = threading.Event()

    def stop(signum: int, _frame: Any) -> None:
        if stopping.is_set():
            return
        stopping.set()
        log(f"[senclaw] signal {signum} — shutting down")
        if on_shutdown:
            try:
                on_shutdown()
            except Exception:
                traceback.print_exc()
        threading.Thread(target=httpd.shutdown, daemon=True).start()

    # SIGTERM is what the daemon sends when it stops an idle session app, and
    # what it sends every app on its own shutdown. Two seconds later it is
    # SIGKILL, so anything unflushed at that point is lost.
    #
    # Only the main thread may install a handler. Serving from another thread is
    # legitimate (tests, an app with its own supervisor), so that is a note
    # rather than a crash — but such an app gets no graceful stop, and saying so
    # is the difference between "my data was lost" and "I knew that".
    try:
        signal.signal(signal.SIGTERM, stop)
        signal.signal(signal.SIGINT, stop)
    except ValueError:
        log(
            "[senclaw] not on the main thread — no SIGTERM handler installed; "
            "this server will be killed without running on_shutdown"
        )

    log(f"[senclaw] listening on http://{listen_host}:{listen_port}")
    try:
        httpd.serve_forever(poll_interval=0.2)
    finally:
        httpd.server_close()
