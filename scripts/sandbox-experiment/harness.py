#!/usr/bin/env python3
"""Sandbox security experiment harness.

Drives the SenClaw daemon's /api/sandbox REST surface (isolated daemon on
127.0.0.1:18990) to run REAL apps under restriction and measure what the
sandbox actually enforces — not what the docs claim.

Every probe states an expectation up front; the report shows measured vs
expected so a silent regression is visible.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("SBX_BASE", "http://127.0.0.1:18990/api/sandbox")
HOST_API = os.environ.get("HOST_API", "http://127.0.0.1:18990")


def api(path, method="GET", body=None, timeout=180):
    url = BASE + path
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read().decode()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        return {"error": e.read().decode()[:400], "status": e.code}


def create(name, **kw):
    body = {"name": name, "backend": "direct"}
    body.update(kw)
    r = api("/sandboxes", "POST", body)
    if "id" not in r:
        raise SystemExit(f"create failed: {r}")
    return r["id"]


def sh(sid, cmd, timeout_ms=60000):
    """Run a shell command inside the sandbox; returns (exit, stdout, stderr)."""
    r = api(f"/sandboxes/{sid}/exec", "POST",
            {"command": cmd, "timeoutMs": timeout_ms})
    if "error" in r:
        return (None, "", r["error"])
    return (r.get("exitCode"), r.get("stdout", ""), r.get("stderr", ""))


RESULTS = []


def probe(app, name, expect, got_allowed, detail=""):
    """expect: True = should be ALLOWED, False = should be BLOCKED."""
    ok = (got_allowed == expect)
    RESULTS.append({
        "app": app, "probe": name,
        "expected": "allow" if expect else "block",
        "measured": "allow" if got_allowed else "block",
        "verdict": "OK" if ok else "MISMATCH",
        "detail": detail.strip()[:220],
    })
    flag = "  " if ok else "!!"
    print(f"{flag} [{app}] {name}: expected={'allow' if expect else 'block'} "
          f"measured={'allow' if got_allowed else 'block'} {detail.strip()[:90]}")
    return ok


def host_get(url, timeout=5):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.status, r.read(400).decode("utf-8", "replace")
    except Exception as e:  # noqa: BLE001 - report whatever went wrong
        return None, str(e)


def wait_port(port, tries=25):
    for _ in range(tries):
        st, _ = host_get(f"http://127.0.0.1:{port}/", timeout=2)
        if st is not None:
            return True
        time.sleep(0.6)
    return False


def cleanup(sid, purge=True):
    api(f"/sandboxes/{sid}?purge={'true' if purge else 'false'}", "DELETE")


def dump(path):
    with open(path, "w") as f:
        json.dump(RESULTS, f, indent=2)
    bad = [r for r in RESULTS if r["verdict"] != "OK"]
    print(f"\n{len(RESULTS)} probes, {len(bad)} mismatches")
    for r in bad:
        print(f"  MISMATCH [{r['app']}] {r['probe']}: {r['detail'][:120]}")
    return len(bad)


if __name__ == "__main__":
    print(json.dumps(api("/caps"))[:200])
