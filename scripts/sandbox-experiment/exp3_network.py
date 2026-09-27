#!/usr/bin/env python3
"""Experiment 3 — what "only local" and "only one website" really buy you.

Measures the outbound matrix across the three network configurations a user can
ask for today, and checks the one thing a sandbox on a developer machine must
not allow: talking to the daemon's own unauthenticated API over loopback.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from harness import api, create, sh, probe, cleanup, dump  # noqa: E402

DAEMON = "127.0.0.1:18990"          # the isolated daemon standing in for a real one
SITE_A = "example.com"
SITE_B = "www.wikipedia.org"        # a second site: proves `connect` is not per-host


def curlable(sid, target, ms=20000):
    """True when the sandbox can complete a TCP+TLS request to `target`."""
    e, out, err = sh(sid, f"curl -s -o /dev/null -m 8 -w '%{{http_code}}' https://{target}/ 2>&1", ms)
    return (e == 0 and out.strip().startswith(("2", "3"))), (out or err).strip()[-60:]


def tcp(sid, host, port, ms=20000):
    e, out, err = sh(sid, f"python3 -c \"import socket;s=socket.socket();s.settimeout(6);"
                          f"s.connect(('{host}',{port}))\" 2>&1", ms)
    return e == 0, (out or err).strip()[-70:]


# ── Config 1: connect:[443] — "HTTPS only" ─────────────────────────────────
sid = create("exp3-https", network=False, fsMode="strict", timeoutMs=60000)
try:
    api(f"/sandboxes/{sid}/ports", "POST", {"listen": [], "connect": [443]})
    ok, d = tcp(sid, "1.1.1.1", 443)
    probe("net", "connect:[443] — a raw TCP connect to :443 is allowed", True, ok, d)
    ok, d = tcp(sid, "1.1.1.1", 53)
    probe("net", "connect:[443] — port 53 stays shut", False, ok, d)
    ok, d = curlable(sid, SITE_A)
    probe("net", "connect:[443] — an HTTPS fetch by hostname now resolves and works",
          True, ok, d)
finally:
    cleanup(sid)

# ── Config 2: connect:[53,443] — "let it resolve and fetch" ────────────────
sid = create("exp3-dns", network=False, fsMode="strict", timeoutMs=60000)
try:
    api(f"/sandboxes/{sid}/ports", "POST", {"listen": [], "connect": [53, 443]})
    ok_a, da = curlable(sid, SITE_A)
    probe("net", f"connect:[53,443] — reaches the intended site ({SITE_A})", True, ok_a, da)
    ok_b, db = curlable(sid, SITE_B)
    # THE point of this experiment: asking for one website does not get you one
    # website. Any host on the opened port is reachable.
    probe("net", f"connect:[53,443] — is a DIFFERENT site ({SITE_B}) blocked?",
          False, ok_b, db)
    ok, d = tcp(sid, "127.0.0.1", 18990)
    probe("net", "connect:[53,443] — the daemon's own API stays out of reach",
          False, ok, d)
finally:
    cleanup(sid)

# ── Config 3: network:true — the coarse switch ─────────────────────────────
sid = create("exp3-open", network=True, fsMode="strict", timeoutMs=60000)
try:
    ok, d = curlable(sid, SITE_A)
    probe("net", "network:true — the internet is reachable (expected)", True, ok, d)

    # File reads of ~/.senclaw are denied by the profile. If the sandbox can
    # call the daemon instead, that denial is worth nothing.
    ok, d = tcp(sid, "127.0.0.1", 18990)
    probe("net", "network:true — daemon API on loopback must NOT be reachable",
          False, ok, d)
    ok, d = tcp(sid, "localhost", 18990)
    probe("net", "network:true — nor by the name 'localhost'", False, ok, d)

    e, out, err = sh(sid, f"curl -s -m 6 http://{DAEMON}/api/sandbox/status | head -c 120 2>&1")
    reached = "sandbox" in (out or "")
    probe("net", "network:true — cannot read the daemon's REST API",
          False, reached, (out or err).strip()[:110])

    # And the most direct consequence: can it drive the sandbox engine itself?
    e, out, err = sh(sid, f"curl -s -m 6 -X POST http://{DAEMON}/api/sandbox/sandboxes "
                          f"-H 'content-type: application/json' "
                          f"-d '{{\"name\":\"spawned-from-inside\",\"backend\":\"direct\"}}' "
                          f"| head -c 120 2>&1")
    spawned = '"id"' in (out or "")
    probe("net", "network:true — cannot create a NEW sandbox through the API",
          False, spawned, (out or err).strip()[:110])
finally:
    cleanup(sid)

# Clean up anything the escalation probe managed to create.
for s in (api("/sandboxes") or {}).get("sandboxes", []):
    if s["name"] == "spawned-from-inside":
        print("NOTE: removing sandbox created from inside the sandbox:", s["id"])
        api(f"/sandboxes/{s['id']}?purge=true", "DELETE")

dump(os.path.join(os.path.dirname(__file__), "exp3_results.json"))
