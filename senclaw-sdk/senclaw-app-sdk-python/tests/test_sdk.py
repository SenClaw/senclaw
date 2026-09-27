"""Tests for the Python Space App SDK.

Two things are worth pinning: that the MCP dispatcher answers exactly what
SenClaw's Rust client sends, and that the manifest validator catches the
silent-failure spellings — because both fail invisibly otherwise.

Run: ``python -m pytest senclaw-sdk/senclaw-app-sdk-python``
"""

from __future__ import annotations

import json
import os
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from senclaw_space import McpServer, error_content, serve, to_content  # noqa: E402
from senclaw_space import manifest as mf  # noqa: E402


# ---------------------------------------------------------------------------
# MCP
# ---------------------------------------------------------------------------


def build() -> McpServer:
    s = McpServer("demo-mcp", "2.0.0")

    @s.tool("demo_echo", "Echo", {"type": "object", "properties": {"text": {"type": "string"}}})
    def echo(args):
        return f"you said {args['text']}"

    @s.tool("demo_add", "Add two numbers")
    def add(**kwargs):
        return {"sum": kwargs["a"] + kwargs["b"]}

    return s


def test_the_three_methods_senclaw_actually_sends():
    s = build()
    init = s.handle({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    assert init["id"] == 1
    assert init["result"]["serverInfo"] == {"name": "demo-mcp", "version": "2.0.0"}
    assert "tools" in init["result"]["capabilities"]

    # SenClaw sends this as a *request* with an id, not a notification, and
    # ignores the reply — but a server that errors on it looks broken in logs.
    note = s.handle({"jsonrpc": "2.0", "id": 2, "method": "notifications/initialized"})
    assert "error" not in note

    listed = s.handle({"jsonrpc": "2.0", "id": 3, "method": "tools/list"})
    names = [t["name"] for t in listed["result"]["tools"]]
    assert names == ["demo_echo", "demo_add"]
    # The schema must survive: a tool with none is one the model guesses at.
    assert listed["result"]["tools"][0]["inputSchema"]["properties"]["text"]["type"] == "string"
    assert all("fn" not in t for t in listed["result"]["tools"]), "never serialise the callable"


def test_both_handler_shapes_work():
    # `def f(args)` and `def f(**kwargs)` are both what people write; picking
    # one and silently failing on the other is the first thing that breaks.
    s = build()
    r = s.handle({"id": 1, "method": "tools/call",
                  "params": {"name": "demo_echo", "arguments": {"text": "hi"}}})
    assert r["result"]["content"][0]["text"] == "you said hi"
    r = s.handle({"id": 2, "method": "tools/call",
                  "params": {"name": "demo_add", "arguments": {"a": 2, "b": 3}}})
    assert json.loads(r["result"]["content"][0]["text"]) == {"sum": 5}


def test_an_unknown_tool_names_the_ones_that_exist():
    s = build()
    r = s.handle({"id": 1, "method": "tools/call", "params": {"name": "nope", "arguments": {}}})
    assert "demo_echo" in r["error"]["message"]


def test_a_raising_tool_becomes_an_error_not_a_dead_server():
    s = McpServer("x")

    @s.tool("x_boom", "Always fails")
    def boom(_args):
        raise ValueError("nope")

    r = s.handle({"id": 1, "method": "tools/call", "params": {"name": "x_boom", "arguments": {}}})
    assert r["error"]["code"] == -32603
    assert "nope" in r["error"]["message"]


def test_content_envelope():
    assert to_content("hi")["content"][0]["text"] == "hi"
    assert json.loads(to_content({"a": 1})["content"][0]["text"]) == {"a": 1}
    # An envelope passes through untouched, so a tool can control its own shape.
    envelope = {"content": [{"type": "text", "text": "x"}], "isError": True}
    assert to_content(envelope) is envelope
    assert error_content("try again")["isError"] is True


# ---------------------------------------------------------------------------
# Manifest
# ---------------------------------------------------------------------------


def test_a_misspelled_mode_is_caught_because_nothing_else_catches_it():
    bad = {"id": "x", "runtime": {"kind": "server", "start": "./x", "mode": "backgroud"}}
    problems = mf.validate(bad)
    assert any("mode" in p and "session" in p for p in problems), problems
    good = {"id": "x", "runtime": {"kind": "server", "start": "./x", "mode": "background"}}
    assert mf.validate(good) == []


def test_an_empty_host_allowlist_means_no_network_at_all():
    bad = {"id": "x", "sandbox": {"network": "hosts", "hosts": []}}
    assert any("no network" in p for p in mf.validate(bad))
    # …and the builder refuses to produce it in the first place.
    try:
        mf.sandbox(network="hosts")
        assert False, "must refuse"
    except ValueError:
        pass


def test_a_server_app_without_a_start_command_is_flagged():
    assert any("start" in p for p in mf.validate({"id": "x", "runtime": {"kind": "server"}}))


def test_the_builders_produce_what_the_daemon_reads():
    m = mf.manifest(
        "demo-py",
        "Demo",
        "A demo",
        runtime_block=mf.runtime("python main.py", 4810, mode="session", runner="python",
                                 health_path="/api/status", idle_timeout_secs=120),
        requires_block=mf.requires(python=">=3.10", bin=["ffmpeg"]),
        sandbox_block=mf.sandbox(force=True, network="hosts", hosts=["api.openai.com"]),
        mcp={"name": "demo-py-mcp", "transport": "http", "path": "/api/mcp/sse",
             "autoRegister": True},
    )
    assert mf.validate(m) == []
    assert m["runtime"]["mode"] == "session"
    assert m["runtime"]["idleTimeoutSecs"] == 120
    assert m["sandbox"]["force"] is True
    assert m["requires"]["bin"] == ["ffmpeg"]


# ---------------------------------------------------------------------------
# The HTTP host — the parts the daemon depends on
# ---------------------------------------------------------------------------


def free_port() -> int:
    import socket

    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def test_health_mcp_and_static_all_answer_on_one_port(tmp_path):
    # This is exactly what the daemon does to an app: waits for healthPath,
    # then POSTs JSON-RPC at mcp.path, then proxies the UI.
    p = free_port()
    os.environ["PORT"] = str(p)
    os.environ.pop("SENCLAW_BIND_HOST", None)
    (tmp_path / "index.html").write_text("<h1>ui</h1>", encoding="utf-8")

    s = build()
    t = threading.Thread(
        target=serve,
        kwargs=dict(
            routes={
                ("POST", "/api/thing"): lambda req: {"got": req.json()},
                ("GET", "/api/status"): lambda _req: {"ok": True, "detail": "from the app"},
            },
            health_path="/api/status",
            mcp_path="/api/mcp/sse",
            mcp_handler=s.handle,
            static_dir=str(tmp_path),
            log=lambda _m: None,
        ),
        daemon=True,
    )
    t.start()

    base = f"http://127.0.0.1:{p}"
    for _ in range(50):
        try:
            urllib.request.urlopen(f"{base}/api/status", timeout=1).read()
            break
        except Exception:
            time.sleep(0.05)
    else:
        raise AssertionError("server never came up")

    # The app's own handler at the health path wins over the built-in one: an
    # app that reports its real state must not be overwritten with `{ok:true}`.
    assert json.loads(urllib.request.urlopen(f"{base}/api/status").read()) == {
        "ok": True,
        "detail": "from the app",
    }

    req = urllib.request.Request(
        f"{base}/api/mcp/sse",
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).encode(),
        headers={"Content-Type": "application/json"},
    )
    body = json.loads(urllib.request.urlopen(req).read())
    assert [t["name"] for t in body["result"]["tools"]] == ["demo_echo", "demo_add"]

    req = urllib.request.Request(
        f"{base}/api/thing",
        data=json.dumps({"x": 1}).encode(),
        headers={"Content-Type": "application/json"},
    )
    assert json.loads(urllib.request.urlopen(req).read()) == {"got": {"x": 1}}

    assert b"ui" in urllib.request.urlopen(f"{base}/").read()
    # An unknown path is a client-side route, not a 404 — SPAs depend on it.
    assert b"ui" in urllib.request.urlopen(f"{base}/some/deep/route").read()


def test_a_static_path_cannot_escape_the_web_root(tmp_path):
    p = free_port()
    os.environ["PORT"] = str(p)
    web = tmp_path / "web"
    web.mkdir()
    (web / "index.html").write_text("ok", encoding="utf-8")
    (tmp_path / "secret.txt").write_text("SECRET", encoding="utf-8")

    threading.Thread(
        target=serve,
        kwargs=dict(health_path="/health", static_dir=str(web), log=lambda _m: None),
        daemon=True,
    ).start()
    base = f"http://127.0.0.1:{p}"
    for _ in range(50):
        try:
            urllib.request.urlopen(f"{base}/health", timeout=1).read()
            break
        except Exception:
            time.sleep(0.05)

    for path in ("/../secret.txt", "/..%2fsecret.txt", "/web/../../secret.txt"):
        try:
            got = urllib.request.urlopen(f"{base}{path}").read()
        except urllib.error.HTTPError as e:
            got = e.read()  # 403/404 is the right answer
        assert b"SECRET" not in got, f"{path} reached outside the web root"


# ---------------------------------------------------------------------------
# The bridge wire contract
# ---------------------------------------------------------------------------
#
# The daemon's `SpaceAppBridgeBody` requires a field named `action` and defines
# no alias for it. Sending anything else — `capability` was the mistake that
# prompted these tests — is a 422 from serde before a line of handler code
# runs, which surfaces to an app author as "the bridge is down" rather than
# "you sent the wrong key". So the key is pinned here rather than trusted.

import http.server  # noqa: E402
import socket  # noqa: E402

from senclaw_space import SenclawError, SenclawSpace  # noqa: E402


class _FakeDaemon:
    """A daemon stub that records what the SDK actually put on the wire."""

    def __init__(self, reply: dict | None = None) -> None:
        self.seen: list[dict] = []
        self.reply = reply if reply is not None else {"status": "ok"}
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        self.port = sock.getsockname()[1]
        sock.close()
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):  # noqa: A003
                return

            def _read(self):
                n = int(self.headers.get("Content-Length") or 0)
                return json.loads(self.rfile.read(n) or b"{}") if n else {}

            def _reply(self, payload):
                body = json.dumps(payload).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                outer.seen.append({"path": self.path, "body": self._read()})
                self._reply(outer.reply)

            def do_GET(self):
                outer.seen.append({"path": self.path, "body": None})
                self._reply(outer.reply)

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", self.port), H)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def client(self) -> SenclawSpace:
        return SenclawSpace(base_url=f"http://127.0.0.1:{self.port}", app_id="t")

    def close(self) -> None:
        self.httpd.shutdown()


def test_bridge_sends_action_not_capability():
    d = _FakeDaemon({"status": "ok", "text": "hi"})
    try:
        d.client().llm("q")
        body = d.seen[0]["body"]
        assert body["action"] == "llm.request", body
        assert "capability" not in body, "the daemon 422s on this"
        assert body["payload"]["prompt"] == "q"
    finally:
        d.close()


def test_llm_detailed_returns_usage_and_finish():
    d = _FakeDaemon({
        "status": "ok", "text": "hello", "model": "m1", "finish": "length",
        "usage": {"inputTokens": 12, "outputTokens": 3, "cacheReadTokens": 9},
    })
    try:
        r = d.client().llm_detailed("q")
        assert (r.text, r.model, r.finish) == ("hello", "m1", "length")
        assert r.usage is not None
        assert r.usage.input_tokens == 12
        assert r.usage.cache_read_tokens == 9
        # Unreported by this provider, and 0 is the right reading of absent.
        assert r.usage.cache_creation_tokens == 0
    finally:
        d.close()


def test_llm_detailed_usage_is_none_when_provider_reports_none():
    # Distinct from "zero tokens" — some local models report nothing at all,
    # and recording that as 0 would quietly understate the daemon's totals.
    d = _FakeDaemon({"status": "ok", "text": "x", "model": "local"})
    try:
        assert d.client().llm_detailed("q").usage is None
    finally:
        d.close()


def test_llm_raises_on_truncated_reply():
    d = _FakeDaemon({"status": "ok", "text": "partial", "finish": "length"})
    try:
        try:
            d.client().llm("q")
        except Exception as exc:
            assert "truncated" in str(exc)
        else:
            raise AssertionError("a truncated reply must not read as a short answer")
    finally:
        d.close()


def test_knowledge_calls_use_the_right_actions():
    d = _FakeDaemon({"status": "ok", "hits": [{"name": "n", "summary": "s", "score": 0.5}]})
    try:
        c = d.client()
        c.knowledge_save("remember this", space="proj", tags=["a"])
        hits = c.knowledge_search("q", space="proj", limit=3)
        assert [x["body"]["action"] for x in d.seen] == ["knowledge.save", "knowledge.search"]
        assert d.seen[0]["body"]["payload"]["tags"] == ["a"]
        assert d.seen[1]["body"]["payload"]["limit"] == 3
        assert hits[0].name == "n" and hits[0].score == 0.5
    finally:
        d.close()


def test_knowledge_omits_space_when_not_given():
    # Omitted means "this app's own private space". Sending space=None would
    # be a different thing to the daemon than not sending the key at all.
    d = _FakeDaemon()
    try:
        d.client().knowledge_save("x")
        assert "space" not in d.seen[0]["body"]["payload"]
    finally:
        d.close()


def test_usage_report_never_raises():
    # Fire-and-forget: accounting must not take down the work it describes.
    c = SenclawSpace(base_url="http://127.0.0.1:9", app_id="t")  # nothing listening
    c.usage_report("m", "p", 1, 2)  # must not raise


def test_list_models_reads_llm_config():
    d = _FakeDaemon({
        "activeId": "a1",
        "configs": [{"id": "a1", "modelName": "Sonnet", "adapt": "anthropic"}, {"nope": 1}],
    })
    try:
        active, models = d.client().list_models()
        assert active == "a1"
        assert len(models) == 1, "an entry with no id is not a model"
        assert (models[0].id, models[0].provider) == ("a1", "anthropic")
        assert d.seen[0]["path"] == "/api/llm-config"
    finally:
        d.close()


# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------

from senclaw_space.dispatch import (  # noqa: E402
    Capacity,
    DispatchProvider,
    WorkItem,
    completed,
    dispatch_routes,
    mcp_stdio,
    workspace_worktree,
)
from senclaw_space.server import Request  # noqa: E402


class _Todos(DispatchProvider):
    def __init__(self) -> None:
        self.finalized: list[tuple[str, dict]] = []
        self.beats: list[str] = []

    def claim_ready(self, capacity: Capacity) -> list[WorkItem]:
        return [
            WorkItem(
                id=f"t{i}",
                prompt="do it",
                assignee="worker",
                mcp=[mcp_stdio("kanban", "senclaw", ["kanban-server"])],
                workspace=workspace_worktree("/repo", "main"),
                depends_on=["t0"] if i else [],
                timeout_secs=60,
            )
            for i in range(capacity.total)
        ]

    def heartbeat(self, item_id: str) -> None:
        self.beats.append(item_id)

    def reclaim(self) -> list[str]:
        return ["stale-1"]

    def finalize(self, item_id: str, outcome: dict) -> None:
        self.finalized.append((item_id, outcome))


def _post(routes, path, payload):
    handler = routes[("POST", path)]
    return handler(Request("POST", path, {}, json.dumps(payload).encode(), {}))


def test_dispatch_poll_serialises_the_rust_wire_shape():
    routes = dispatch_routes(_Todos())
    resp = _post(routes, "/api/dispatch/poll", {"capacity": {"total": 2, "per_assignee": 1}})
    items = json.loads(resp.body)
    assert len(items) == 2
    it = items[1]
    # snake_case, exactly as serde expects — camelCase is dropped silently,
    # which would surface as a dependency that never held.
    assert it["depends_on"] == ["t0"]
    assert it["timeout_secs"] == 60
    assert it["workspace"] == {"kind": "worktree", "repo": "/repo", "branch": "main"}
    assert it["mcp"][0]["transport"] == "stdio"
    assert it["mcp"][0]["args"] == ["kanban-server"]


def test_dispatch_finalize_and_heartbeat_and_reclaim():
    p = _Todos()
    routes = dispatch_routes(p)
    _post(routes, "/api/dispatch/heartbeat", {"item_id": "t1"})
    assert p.beats == ["t1"]
    assert json.loads(_post(routes, "/api/dispatch/reclaim", {}).body) == ["stale-1"]
    _post(routes, "/api/dispatch/finalize", {"item_id": "t1", "outcome": completed("done", {"n": 1})})
    assert p.finalized == [("t1", {"status": "completed", "summary": "done", "metadata": {"n": 1}})]


def test_dispatch_provider_error_becomes_500_not_a_reset():
    class Broken(DispatchProvider):
        def claim_ready(self, capacity):
            raise RuntimeError("db is gone")

        def finalize(self, item_id, outcome):
            pass

    resp = _post(dispatch_routes(Broken()), "/api/dispatch/poll", {})
    assert resp.status == 500
    assert json.loads(resp.body)["error"] == "db is gone"


def test_dispatch_prefix_is_configurable():
    routes = dispatch_routes(_Todos(), prefix="custom/queue")
    assert ("POST", "/custom/queue/poll") in routes


def test_bridge_error_envelope_raises_despite_http_200():
    # The daemon answers a failed action with HTTP 200 and status:"error".
    # Reading only the HTTP code turns a dead provider into an empty string,
    # which downstream reads as "the model had nothing to say".
    d = _FakeDaemon({"status": "error", "message": "LLM HTTP 404 Not Found"})
    try:
        try:
            d.client().llm("q")
        except SenclawError as exc:
            assert "404" in str(exc)
        else:
            raise AssertionError("a failed bridge action must not look like an empty reply")
    finally:
        d.close()


def test_bridge_pending_names_the_real_problem():
    d = _FakeDaemon({"status": "pending"})
    try:
        try:
            d.client().knowledge_recall("q")
        except SenclawError as exc:
            assert "not enabled" in str(exc)
        else:
            raise AssertionError("pending must not read as success")
    finally:
        d.close()


def test_bridge_passes_through_a_payload_with_no_status_field():
    # Not every action answers with an envelope; those must not be mistaken
    # for failures just because `status` is absent.
    d = _FakeDaemon({"hits": []})
    try:
        assert d.client().knowledge_search("q") == []
    finally:
        d.close()


# ---------------------------------------------------------------------------
# App access token — the app's identity, both directions
# ---------------------------------------------------------------------------

from senclaw_space import (  # noqa: E402
    ENV_APP_TOKEN,
    HEADER_APP_TOKEN,
    HEADER_API_VERSION,
    api_version_from_env,
)

_TOKEN = "sca_" + "a" * 64


class _HeaderRecordingDaemon:
    """Like _FakeDaemon, but keeps the request headers — which is the whole
    point here: the token is invisible in the body."""

    def __init__(self) -> None:
        self.headers: list[dict] = []
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        self.port = sock.getsockname()[1]
        sock.close()
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):  # noqa: A003
                return

            def _reply(self):
                outer.headers.append({k.lower(): v for k, v in self.headers.items()})
                body = b'{"items":[]}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_GET = _reply
            do_POST = _reply

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", self.port), H)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()


def test_client_sends_token_and_version_on_every_call():
    d = _HeaderRecordingDaemon()
    try:
        space = SenclawSpace(
            app_id="t", base_url=f"http://127.0.0.1:{d.port}", app_token=_TOKEN
        )
        space.list_config()
        sent = d.headers[0]
        assert sent[HEADER_APP_TOKEN.lower()] == _TOKEN
        assert sent[HEADER_API_VERSION.lower()], "the daemon cannot negotiate an unstated contract"
    finally:
        d.close()


def test_client_omits_an_empty_token_header():
    # Running the app by hand: nothing in the environment. Sending the header
    # blank would make the daemon try to resolve "" and refuse a call its
    # default mode would have served.
    d = _HeaderRecordingDaemon()
    try:
        space = SenclawSpace(app_id="t", base_url=f"http://127.0.0.1:{d.port}", app_token="")
        space.list_config()
        assert HEADER_APP_TOKEN.lower() not in d.headers[0]
    finally:
        d.close()


def test_api_version_from_env_ignores_garbage():
    old = os.environ.get("SENCLAW_API_VERSION")
    try:
        os.environ["SENCLAW_API_VERSION"] = "7"
        assert api_version_from_env() == 7
        # A non-numeric value must not become 0, which would drop the header.
        os.environ["SENCLAW_API_VERSION"] = "v2"
        assert api_version_from_env() >= 1
    finally:
        if old is None:
            os.environ.pop("SENCLAW_API_VERSION", None)
        else:
            os.environ["SENCLAW_API_VERSION"] = old


def test_serve_guard_refuses_everyone_but_the_daemon():
    old = os.environ.get(ENV_APP_TOKEN)
    os.environ[ENV_APP_TOKEN] = _TOKEN
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    p = sock.getsockname()[1]
    sock.close()
    os.environ["PORT"] = str(p)
    t = threading.Thread(
        target=serve,
        args=({("GET", "/api/notes"): lambda req: {"ok": True}},),
        kwargs={
            "health_path": "/api/status",
            "require_app_token": True,
            "auth_skip_paths": ["/public/*"],
            "log": lambda m: None,
        },
        daemon=True,
    )
    t.start()
    time.sleep(0.4)

    def call(path: str, token: str | None) -> int:
        req = urllib.request.Request(f"http://127.0.0.1:{p}{path}")
        if token:
            req.add_header(HEADER_APP_TOKEN, token)
        try:
            with urllib.request.urlopen(req, timeout=3) as r:
                return r.status
        except urllib.error.HTTPError as e:
            return e.code

    try:
        assert call("/api/notes", _TOKEN) == 200, "the daemon's own request must pass"
        # What the guard exists to stop: another local process on the port.
        assert call("/api/notes", None) == 401
        assert call("/api/notes", "sca_" + "f" * 64) == 401
        # The daemon's health check runs before anything is proxied; locking it
        # out would make the app look permanently dead.
        assert call("/api/status", None) == 200
        assert call("/public/logo.png", None) in (200, 404), "skipped prefix must not 401"
    finally:
        if old is None:
            os.environ.pop(ENV_APP_TOKEN, None)
        else:
            os.environ[ENV_APP_TOKEN] = old
        os.environ.pop("PORT", None)
