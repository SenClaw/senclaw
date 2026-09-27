"""Exposing a Python Space App's tools to SenClaw agents over MCP.

SenClaw's MCP client for an app is deliberately plain: JSON-RPC 2.0 objects
POSTed to one URL, one request per response, plain ``application/json`` back.
There is no session, no SSE stream and no long-lived connection to manage, so
the whole server is a dictionary of handlers — which is why this has no
dependency on the MCP SDK.

Three methods are all a client ever sends:

* ``initialize``  → who you are and what you support
* ``tools/list``  → the tools, with their JSON Schemas
* ``tools/call``  → run one

``notifications/initialized`` arrives too and is answered with an empty result;
SenClaw sends it as a request rather than a notification and ignores the reply.

The tool **names** are what agents type, so they follow the repo convention:
``<prefix>_<verb>[_<modifier>]`` in snake_case, reached by the agent as
``mcp__<mcp.name>__<tool>``.
"""

from __future__ import annotations

import inspect
import json
import traceback
from typing import Any, Callable

ToolFn = Callable[[dict[str, Any]], Any]

PROTOCOL_VERSION = "2024-11-05"


class McpServer:
    """A registry of tools, and the JSON-RPC dispatcher over them."""

    def __init__(self, name: str, version: str = "1.0.0") -> None:
        self.name = name
        self.version = version
        self._tools: dict[str, dict[str, Any]] = {}

    # -- registration -----------------------------------------------------

    def tool(
        self,
        name: str,
        description: str,
        input_schema: dict[str, Any] | None = None,
    ) -> Callable[[ToolFn], ToolFn]:
        """Decorator registering one tool.

        ``input_schema`` is the JSON Schema the agent sees. Write it: a tool
        with no schema is one the model has to guess the arguments of, and it
        guesses badly.
        """

        def wrap(fn: ToolFn) -> ToolFn:
            self._tools[name] = {
                "name": name,
                "description": description,
                "inputSchema": input_schema
                or {"type": "object", "properties": {}, "additionalProperties": True},
                "fn": fn,
            }
            return fn

        return wrap

    def add_tool(
        self,
        name: str,
        description: str,
        fn: ToolFn,
        input_schema: dict[str, Any] | None = None,
    ) -> None:
        """Register a tool without the decorator."""
        self.tool(name, description, input_schema)(fn)

    @property
    def tool_names(self) -> list[str]:
        return sorted(self._tools)

    # -- dispatch ---------------------------------------------------------

    def handle(self, request: dict[str, Any]) -> dict[str, Any]:
        """One JSON-RPC request in, one JSON-RPC response out."""
        rpc_id = request.get("id")
        method = request.get("method") or ""
        params = request.get("params") or {}
        try:
            result = self._dispatch(method, params)
        except _RpcError as e:
            return {"jsonrpc": "2.0", "id": rpc_id, "error": {"code": e.code, "message": str(e)}}
        except Exception as e:
            traceback.print_exc()
            return {
                "jsonrpc": "2.0",
                "id": rpc_id,
                "error": {"code": -32603, "message": f"{type(e).__name__}: {e}"},
            }
        return {"jsonrpc": "2.0", "id": rpc_id, "result": result}

    def _dispatch(self, method: str, params: dict[str, Any]) -> Any:
        if method == "initialize":
            return {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": self.name, "version": self.version},
            }
        if method in ("notifications/initialized", "initialized", "ping"):
            return {}
        if method == "tools/list":
            return {
                "tools": [
                    {k: v for k, v in t.items() if k != "fn"} for t in self._tools.values()
                ]
            }
        if method == "tools/call":
            return self._call(params.get("name") or "", params.get("arguments") or {})
        raise _RpcError(-32601, f"method not found: {method}")

    def _call(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        tool = self._tools.get(name)
        if tool is None:
            raise _RpcError(-32602, f"unknown tool: {name} (have: {', '.join(self.tool_names)})")
        fn = tool["fn"]
        # Accept both shapes an author naturally writes: `def f(args)` and
        # `def f(**kwargs)`. Getting this wrong is the first thing that breaks
        # when someone copies a tool from another codebase.
        sig = inspect.signature(fn)
        takes_kwargs = any(
            p.kind is inspect.Parameter.VAR_KEYWORD for p in sig.parameters.values()
        )
        out = fn(**arguments) if takes_kwargs else fn(arguments)
        return to_content(out)


class _RpcError(Exception):
    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code


def to_content(value: Any) -> dict[str, Any]:
    """Wrap a tool's return value in the MCP content envelope.

    A tool may return a string, a JSON-able object, or the envelope itself when
    it wants control. Anything else is JSON-encoded — an agent reads text, so
    returning a bare object that cannot be serialised is a silent nothing.
    """
    if isinstance(value, dict) and "content" in value:
        return value
    if isinstance(value, str):
        text = value
    else:
        text = json.dumps(value, ensure_ascii=False, indent=2, default=str)
    return {"content": [{"type": "text", "text": text}]}


def error_content(message: str) -> dict[str, Any]:
    """A tool failure the agent can read and act on.

    Returned as content with ``isError``, not raised: a JSON-RPC error is a
    transport failure and the agent sees a stack trace, where what it needs is
    the sentence explaining what to do differently.
    """
    return {"content": [{"type": "text", "text": message}], "isError": True}
