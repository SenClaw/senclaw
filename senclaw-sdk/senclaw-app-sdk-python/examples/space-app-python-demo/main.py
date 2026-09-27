"""A complete Space App in Python, in one file.

What it demonstrates, in the order the daemon exercises it:

1. `requires.python` is checked before this file is ever executed.
2. There is no `requirements.txt`, so no venv is built and no install runs —
   the app starts as fast as Python boots. Add one and the daemon creates
   `.venv` in this directory and installs into it.
3. `runtime.mode: "session"` — the daemon does *not* start this at boot. It
   starts when the user opens the app, or when an agent calls one of the tools
   below, and stops it again 60 seconds after the last request.
4. Its two MCP tools are in every agent's roster even while it is stopped: the
   tool list is cached, and the MCP URL points at the daemon's proxy, which
   starts the app before forwarding the call.

Run it by hand for development:

    SENCLAW_SPACE_APP_ID=python-demo PORT=4810 python main.py
"""

from __future__ import annotations

import platform
import sys
import time

from senclaw_space import McpServer, Request, SenclawSpace, error_content, serve

APP_ID = "python-demo"
STARTED_AT = time.time()

# `SenclawSpace()` reads SENCLAW_SPACE_APP_ID and SENCLAW_BASE_URL from the
# environment the daemon sets. The explicit id keeps a bare `python main.py`
# working during development.
space = SenclawSpace(app_id=APP_ID)
mcp = McpServer("python-demo-mcp")


@mcp.tool(
    "pydemo_env",
    "Report the Python runtime this Space App is running on",
    {"type": "object", "properties": {}},
)
def env(_args: dict) -> dict:
    return {
        "python": sys.version.split()[0],
        "executable": sys.executable,
        "platform": platform.platform(),
        # Non-empty when the daemon built a venv for this app — the tell that
        # dependency isolation actually happened.
        "venv": sys.prefix if sys.prefix != sys.base_prefix else None,
        "uptimeSecs": round(time.time() - STARTED_AT, 1),
    }


@mcp.tool(
    "pydemo_summarise",
    "Summarise a piece of text in three sentences",
    {
        "type": "object",
        "properties": {"text": {"type": "string", "description": "The text to summarise"}},
        "required": ["text"],
    },
)
def summarise(args: dict) -> str:
    text = (args.get("text") or "").strip()
    if not text:
        # A readable sentence, not a stack trace: the agent has to know what to
        # do differently, and a JSON-RPC error tells it nothing.
        return error_content("`text` is empty — pass the text to summarise.")
    # The app holds no provider key. This goes to the daemon, which uses
    # whichever provider the user configured.
    return space.llm(
        f"Summarise the following in exactly three sentences:\n\n{text}",
        max_tokens=600,
    )


def status(_req: Request) -> dict:
    """`runtime.healthPath`. The daemon waits on this before it calls the app
    started, and polls it afterwards, so it must stay cheap and never block."""
    return {"ok": True, "app": APP_ID, "uptimeSecs": round(time.time() - STARTED_AT, 1)}


def counter(_req: Request) -> dict:
    """Shows the config KV: the same store the app's own UI reads and writes,
    which is why settings belong here and not in a file an update would
    overwrite."""
    n = int(space.get_config("visits", 0) or 0) + 1
    space.set_config("visits", n)
    return {"visits": n}


if __name__ == "__main__":
    serve(
        {
            ("GET", "/api/status"): status,
            ("POST", "/api/visit"): counter,
        },
        health_path="/api/status",
        mcp_path="/api/mcp/sse",
        mcp_handler=mcp.handle,
        static_dir="web",
        # A session app is stopped with SIGTERM and killed two seconds later.
        on_shutdown=lambda: print("[python-demo] flushing before exit"),
        default_port=4810,
    )
