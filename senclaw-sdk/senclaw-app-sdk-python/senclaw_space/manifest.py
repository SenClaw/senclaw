"""Writing a ``senclaw-manifest.json`` that says what you meant.

The manifest is what the daemon reads to decide how the app runs, and every
field it does not understand is silently ignored — a misspelled ``mode`` makes
an always-on app on-demand without a word anywhere. So this builds the file
from named arguments and validates the values that have a fixed set.

Nothing here is required to write a Space App. It is here so the fields are
discoverable from Python, and so ``python -m senclaw_space.manifest`` can check
a hand-written file.
"""

from __future__ import annotations

import json
import sys
from typing import Any, Iterable, Literal

RunMode = Literal["background", "session"]
Runner = Literal["binary", "node", "python", "shell"]
ReadMode = Literal["open", "strict", "allowlist"]
Network = Literal["off", "all", "hosts"]

VALID_MODES = ("background", "session")
VALID_RUNNERS = ("binary", "node", "python", "shell")
VALID_READ_MODES = ("open", "strict", "allowlist")
VALID_NETWORKS = ("off", "all", "hosts")


def runtime(
    start: str,
    port: int,
    *,
    mode: RunMode = "session",
    health_path: str = "/health",
    runner: Runner | None = None,
    idle_timeout_secs: int | None = None,
    install: str | None = None,
    venv: bool | None = None,
) -> dict[str, Any]:
    """The ``runtime`` block.

    ``mode="background"`` is for an app that does work nobody asked for at that
    moment — polls a channel, runs a schedule, holds a WebSocket a browser
    extension dials into. Everything else is ``session``: started when it is
    opened or one of its tools is called, stopped once idle. Choosing
    ``background`` for an app that does not need it means a resident process
    for as long as SenClaw runs.
    """
    if mode not in VALID_MODES:
        raise ValueError(f"mode must be one of {VALID_MODES}, got {mode!r}")
    if runner is not None and runner not in VALID_RUNNERS:
        raise ValueError(f"runner must be one of {VALID_RUNNERS}, got {runner!r}")
    block: dict[str, Any] = {"kind": "server", "mode": mode, "start": start,
                             "healthPath": health_path, "port": port}
    if runner:
        block["runner"] = runner
    if idle_timeout_secs is not None:
        block["idleTimeoutSecs"] = idle_timeout_secs
    if install:
        block["install"] = install
    if venv is not None:
        block["venv"] = venv
    return block


def requires(
    *,
    node: str | None = None,
    python: str | None = None,
    bin: Iterable[str] = (),
    optional_bin: Iterable[str] = (),
    env: Iterable[str] = (),
    os_: Iterable[str] = (),
) -> dict[str, Any]:
    """The ``requires`` block — what the *machine* must provide.

    Checked at install and again before every launch, so "install ffmpeg" is a
    sentence the user reads instead of ``exit 127`` in a log file. Version
    ranges are the ordinary ones: ``>=18``, ``>=3.10 <4``, ``^18``, ``~3.10``,
    ``18.x``.
    """
    out: dict[str, Any] = {}
    if node:
        out["node"] = node
    if python:
        out["python"] = python
    if bin:
        out["bin"] = list(bin)
    if optional_bin:
        out["optionalBin"] = list(optional_bin)
    if env:
        out["env"] = list(env)
    if os_:
        out["os"] = list(os_)
    return out


def sandbox(
    *,
    force: bool = False,
    enabled: bool | None = None,
    read_mode: ReadMode | None = None,
    network: Network | None = None,
    hosts: Iterable[str] = (),
    daemon_api: bool | None = None,
    loopback: Iterable[int] = (),
    folders: Iterable[dict[str, Any] | str] = (),
) -> dict[str, Any]:
    """The ``sandbox`` block — the confinement the app asks for itself.

    Applied at install. ``force=True`` also means the user cannot switch the
    sandbox off from Plugins → Space Apps, which is the right declaration for
    an app whose whole point is that it is confined — and the wrong one for an
    app that merely prefers it.

    ``network="hosts"`` needs ``hosts``, and it is enforced by an allowlisting
    proxy rather than an OS rule: no sandbox here can filter by hostname. A
    client that ignores ``HTTP_PROXY`` therefore reaches *nothing*, not
    everything — so test the app with it on before shipping the declaration.
    """
    if read_mode is not None and read_mode not in VALID_READ_MODES:
        raise ValueError(f"read_mode must be one of {VALID_READ_MODES}")
    if network is not None and network not in VALID_NETWORKS:
        raise ValueError(f"network must be one of {VALID_NETWORKS}")
    if network == "hosts" and not list(hosts):
        raise ValueError('network="hosts" with an empty host list gives the app no network at all')
    out: dict[str, Any] = {"force": force}
    if enabled is not None:
        out["enabled"] = enabled
    if read_mode:
        out["readMode"] = read_mode
    if network:
        out["network"] = network
    if hosts:
        out["hosts"] = list(hosts)
    if daemon_api is not None:
        out["daemonApi"] = daemon_api
    if loopback:
        out["loopback"] = list(loopback)
    if folders:
        out["folders"] = [{"path": f} if isinstance(f, str) else f for f in folders]
    return out


def manifest(
    app_id: str,
    name: str,
    description: str,
    *,
    icon: str = "🐍",
    runtime_block: dict[str, Any],
    mcp: dict[str, Any] | None = None,
    requires_block: dict[str, Any] | None = None,
    sandbox_block: dict[str, Any] | None = None,
    integration: dict[str, Any] | None = None,
) -> dict[str, Any]:
    out: dict[str, Any] = {
        "id": app_id,
        "name": name,
        "description": description,
        "icon": icon,
        "runtime": runtime_block,
        "integration": integration or {"type": "iframe", "url": "/"},
    }
    if mcp:
        out["mcp"] = mcp
    if requires_block:
        out["requires"] = requires_block
    if sandbox_block:
        out["sandbox"] = sandbox_block
    return out


def validate(data: dict[str, Any]) -> list[str]:
    """Problems found in a manifest, most important first. Empty means fine."""
    problems: list[str] = []
    if not data.get("id"):
        problems.append("missing `id`")
    rt = data.get("runtime")
    if isinstance(rt, dict):
        if rt.get("kind") == "server" and not rt.get("start"):
            problems.append("runtime.kind is `server` but there is no `start` command")
        mode = rt.get("mode")
        if mode is not None and mode not in VALID_MODES:
            problems.append(
                f"runtime.mode = {mode!r} is not understood — it will be treated as `session`, "
                f"so an always-on app would silently stop when idle. Use one of {VALID_MODES}."
            )
        runner = rt.get("runner")
        if runner is not None and runner not in VALID_RUNNERS:
            problems.append(f"runtime.runner = {runner!r}; use one of {VALID_RUNNERS}")
    elif rt is not None:
        problems.append("`runtime` must be an object")
    sb = data.get("sandbox")
    if isinstance(sb, dict):
        if sb.get("network") == "hosts" and not sb.get("hosts"):
            problems.append('sandbox.network is "hosts" but `hosts` is empty — the app gets no network')
        if sb.get("readMode") not in (None, *VALID_READ_MODES):
            problems.append(f"sandbox.readMode must be one of {VALID_READ_MODES}")
    mcp_block = data.get("mcp")
    if isinstance(mcp_block, dict) and mcp_block.get("autoRegister") and not mcp_block.get("path"):
        if not mcp_block.get("url"):
            problems.append("mcp.autoRegister is set but there is neither `path` nor `url`")
    return problems


def _main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: python -m senclaw_space.manifest <senclaw-manifest.json>", file=sys.stderr)
        return 2
    with open(argv[1], encoding="utf-8") as f:
        data = json.load(f)
    problems = validate(data)
    for p in problems:
        print(f"✗ {p}")
    if not problems:
        rt = data.get("runtime") or {}
        print(f"✓ {data.get('id')}: mode={rt.get('mode', 'session')} runner={rt.get('runner', 'auto')}")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv))
