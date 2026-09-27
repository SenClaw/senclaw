"""Autonomous work dispatch — the app side of the contract.

The daemon's ``MCPDispatcher`` engine can drive any app that exposes four
endpoints. Implement :class:`DispatchProvider` over your own store, hand it to
:func:`dispatch_routes`, and the engine will claim work from you, keep leases
alive, recover items whose worker died, and report terminal outcomes back.

    from senclaw_space import serve
    from senclaw_space.dispatch import DispatchProvider, dispatch_routes

    class Todos(DispatchProvider):
        def claim_ready(self, capacity): ...
        def finalize(self, item_id, outcome): ...

    serve(routes={**dispatch_routes(Todos()), ("GET", "/api/status"): status})

The wire shape is the Rust SDK's, field for field, because the same engine
parses both: snake_case JSON, ``Outcome`` tagged by ``status``, ``Workspace``
tagged by ``kind``, ``McpServerSpec`` tagged by ``transport``.
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from typing import Any, Callable

from .server import Request, Response

# ---------------------------------------------------------------------------
# wire types
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Capacity:
    """How many workers the dispatcher can spawn right now."""

    #: Max items to claim across this source this tick.
    total: int = 0
    #: Max concurrent items per assignee (worker lane). 0 = unlimited.
    per_assignee: int = 0

    @staticmethod
    def from_json(d: Any) -> "Capacity":
        if not isinstance(d, dict):
            return Capacity()
        return Capacity(
            total=int(d.get("total") or 0),
            per_assignee=int(d.get("per_assignee") or 0),
        )


def workspace_scratch() -> dict[str, Any]:
    """Fresh temp dir, discarded when the worker finishes."""
    return {"kind": "scratch"}


def workspace_dir(path: str) -> dict[str, Any]:
    """A persistent absolute path."""
    return {"kind": "dir", "path": path}


def workspace_worktree(repo: str, branch: str | None = None) -> dict[str, Any]:
    """A git worktree, for coding tasks."""
    out: dict[str, Any] = {"kind": "worktree", "repo": repo}
    if branch:
        out["branch"] = branch
    return out


def mcp_stdio(
    name: str,
    command: str,
    args: list[str] | None = None,
    env: dict[str, str] | None = None,
) -> dict[str, Any]:
    """A native stdio MCP server the worker should get.

    Prefer this over :func:`mcp_http` — an HTTP spec has to be bridged to stdio
    by the engine at launch, which is one more process and one more failure mode.
    """
    return {
        "transport": "stdio",
        "name": name,
        "command": command,
        "args": args or [],
        "env": env or {},
    }


def mcp_http(name: str, url: str) -> dict[str, Any]:
    """An HTTP/SSE MCP server — e.g. this app's own ``/api/mcp/sse``."""
    return {"transport": "http", "name": name, "url": url}


@dataclass
class WorkItem:
    """A single dispatchable unit of work."""

    #: Source-scoped id, opaque to the engine.
    id: str
    #: The task to run — becomes the agent's user prompt.
    prompt: str
    #: Worker/persona to route to. ``None`` = the source's default persona.
    assignee: str | None = None
    #: Extra system-prompt block appended to the persona's own.
    guidance: str | None = None
    #: MCP servers the worker gets, usually including this app's own tools.
    mcp: list[dict[str, Any]] = field(default_factory=list)
    #: Where the worker runs — one of the ``workspace_*`` helpers.
    workspace: dict[str, Any] = field(default_factory=workspace_scratch)
    #: Ids this item depends on. Already satisfied by the time you return it.
    depends_on: list[str] = field(default_factory=list)
    #: Higher runs first.
    priority: int = 0
    #: Per-item run timeout.
    timeout_secs: int | None = None

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


def completed(summary: str = "", metadata: Any = None) -> dict[str, Any]:
    """Terminal outcome: the work is done."""
    return {"status": "completed", "summary": summary, "metadata": metadata}


def blocked(reason: str) -> dict[str, Any]:
    """Terminal outcome: the worker cannot proceed and a human must look."""
    return {"status": "blocked", "reason": reason}


def failed(error: str) -> dict[str, Any]:
    """Terminal outcome: the work was attempted and did not succeed."""
    return {"status": "failed", "error": error}


def timed_out() -> dict[str, Any]:
    """Terminal outcome: the worker ran past its timeout."""
    return {"status": "timed_out"}


# ---------------------------------------------------------------------------
# provider
# ---------------------------------------------------------------------------


class DispatchProvider:
    """Implement over your own store. Subclass and override.

    Only :meth:`claim_ready` and :meth:`finalize` are mandatory in practice;
    the other two have sane no-op defaults for a source with no lease model.
    """

    def claim_ready(self, capacity: Capacity) -> list[WorkItem]:
        """Atomically claim up to ``capacity`` ready items.

        **Atomically** matters: the engine may poll again before these items
        finish, and an item handed out twice is run twice.
        """
        raise NotImplementedError

    def heartbeat(self, item_id: str) -> None:
        """Extend the lease on an in-flight item. No-op if you have no leases."""
        return None

    def reclaim(self) -> list[str]:
        """Return dead-worker/expired-lease items to ready; return their ids."""
        return []

    def finalize(self, item_id: str, outcome: dict[str, Any]) -> None:
        """Record a terminal outcome. Map it to your own states."""
        raise NotImplementedError


def dispatch_routes(
    provider: DispatchProvider, prefix: str = "/api/dispatch"
) -> dict[tuple[str, str], Callable[[Request], Response]]:
    """Build the four routes, ready to merge into :func:`senclaw_space.serve`.

    ``POST {prefix}/poll``, ``/heartbeat``, ``/reclaim``, ``/finalize`` — the
    same paths and payloads the Rust ``dispatch_router`` serves.
    """
    prefix = "/" + prefix.strip("/")

    def _body(req: Request) -> dict[str, Any]:
        try:
            data = req.json()
        except (ValueError, json.JSONDecodeError):
            return {}
        return data if isinstance(data, dict) else {}

    def _err(exc: Exception) -> Response:
        # The engine reads `error` and backs off. A stack trace here would be
        # printed into the daemon's log with no way to correlate it.
        return Response({"error": str(exc)}, status=500)

    def poll(req: Request) -> Response:
        try:
            items = provider.claim_ready(Capacity.from_json(_body(req).get("capacity")))
            return Response([i.to_json() if isinstance(i, WorkItem) else i for i in items])
        except Exception as exc:  # noqa: BLE001
            return _err(exc)

    def heartbeat(req: Request) -> Response:
        try:
            provider.heartbeat(str(_body(req).get("item_id") or ""))
            return Response({"ok": True})
        except Exception as exc:  # noqa: BLE001
            return _err(exc)

    def reclaim(_req: Request) -> Response:
        try:
            return Response(provider.reclaim())
        except Exception as exc:  # noqa: BLE001
            return _err(exc)

    def finalize(req: Request) -> Response:
        body = _body(req)
        try:
            outcome = body.get("outcome")
            provider.finalize(
                str(body.get("item_id") or ""),
                outcome if isinstance(outcome, dict) else failed("no outcome sent"),
            )
            return Response({"ok": True})
        except Exception as exc:  # noqa: BLE001
            return _err(exc)

    return {
        ("POST", f"{prefix}/poll"): poll,
        ("POST", f"{prefix}/heartbeat"): heartbeat,
        ("POST", f"{prefix}/reclaim"): reclaim,
        ("POST", f"{prefix}/finalize"): finalize,
    }
