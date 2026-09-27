#!/usr/bin/env python3
"""Experiment 1 — a real file-serving app under path + port restriction.

Subject: Python stdlib `http.server` serving a mounted folder.
Restrictions asked for: read ONE folder, serve locally on ONE port, no internet.

What we are testing is not the app; it is whether the sandbox's claims hold
while the app still does its job.
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from harness import (api, create, sh, probe, host_get, wait_port, cleanup,  # noqa: E402
                     dump, RESULTS)

APP = "http.server"
HOME = os.path.expanduser("~")
SHARED = "/private/tmp/claude-501/-Users-benji-Projects-SemaClaw/9a506870-d4fc-428c-b738-c057abe289f0/scratchpad/exp/shared"
SECRET = "/private/tmp/claude-501/-Users-benji-Projects-SemaClaw/9a506870-d4fc-428c-b738-c057abe289f0/scratchpad/exp/secret"
PORT = 8099
CLOSED_PORT = 8098

os.makedirs(SHARED, exist_ok=True)
os.makedirs(SECRET, exist_ok=True)
open(os.path.join(SHARED, "index.html"), "w").write("<h1>served from the mount</h1>")
open(os.path.join(SECRET, "not-for-the-app.txt"), "w").write("PRIVATE-CANARY-9421")

sid = create("exp1-static", network=False, fsMode="strict",
             timeoutMs=120000, memoryMb=1024)
print("sandbox:", sid)
try:
    # Grant exactly one folder, read-only, and exactly one listening port.
    print(api(f"/sandboxes/{sid}/mounts", "POST",
              {"source": SHARED, "readOnly": True}))
    print(api(f"/sandboxes/{sid}/ports", "POST",
              {"listen": [PORT], "connect": []}))

    # ── the app itself has to work ─────────────────────────────────────────
    sh(sid, f"nohup python3 -m http.server {PORT} --directory "
            f"'{SHARED}' > server.log 2>&1 & sleep 2; echo started", 30000)
    up = wait_port(PORT)
    st, body = host_get(f"http://127.0.0.1:{PORT}/index.html")
    probe(APP, "app serves its content on the opened port",
          True, up and st == 200 and "served from the mount" in body,
          f"HTTP {st}: {body[:60]}")

    # ── path restrictions ──────────────────────────────────────────────────
    e, out, _ = sh(sid, f"cat '{SHARED}/index.html'")
    probe(APP, "reads the folder it was given", True,
          e == 0 and "served from the mount" in out, out[:80])

    e, out, err = sh(sid, f"cat '{SECRET}/not-for-the-app.txt'")
    probe(APP, "cannot read a sibling folder it was not given", False,
          e == 0 and "PRIVATE-CANARY" in out, (out or err)[:100])

    e, out, err = sh(sid, f"ls {HOME}/.ssh 2>&1 | head -3; cat {HOME}/.senclaw/oauth.json 2>&1 | head -c 60")
    probe(APP, "cannot read credential stores (~/.ssh, oauth.json)", False,
          "BEGIN" in out or "access_token" in out or "id_rsa" in out, out[:100])

    e, out, err = sh(sid, "echo escaped > /Users/benji/Projects/SemaClaw/SANDBOX_ESCAPE.txt")
    escaped = os.path.exists("/Users/benji/Projects/SemaClaw/SANDBOX_ESCAPE.txt")
    probe(APP, "cannot write into the user's project", False, escaped,
          (err or out)[:100])

    e, out, _ = sh(sid, "echo inside > work.txt && cat work.txt")
    probe(APP, "can write inside its own workspace", True,
          e == 0 and "inside" in out, out[:60])

    # A read-only mount must refuse writes even though it is granted.
    e, out, err = sh(sid, f"echo tampered > '{SHARED}/index.html'")
    tampered = "tampered" in open(os.path.join(SHARED, "index.html")).read()
    probe(APP, "read-only mount refuses writes", False, tampered, (err or out)[:100])

    # ── network restrictions ───────────────────────────────────────────────
    # Detect by EXIT CODE, never by a marker string: a Python traceback echoes
    # the failing source line, so grepping stdout for the marker reports a
    # blocked call as if it succeeded (this harness had that bug).
    e, out, err = sh(sid, f"python3 -c \"import socket;s=socket.socket();"
                          f"s.bind(('127.0.0.1',{CLOSED_PORT}))\" 2>&1")
    probe(APP, "cannot bind a port that was not opened", False, e == 0, out[-100:])

    e, out, err = sh(sid, "python3 -c \"import socket;s=socket.socket();s.settimeout(5);"
                          "s.connect(('1.1.1.1',443))\" 2>&1")
    probe(APP, "cannot reach the internet (no connect ports)", False, e == 0, out[-100:])

    e, out, err = sh(sid, "python3 -c \"import socket;s=socket.socket();s.settimeout(5);"
                          "s.connect(('8.8.8.8',53))\" 2>&1")
    probe(APP, "cannot reach DNS either", False, e == 0, out[-100:])

    # The interesting one: can the sandboxed app reach the SenClaw daemon's own
    # unauthenticated API on loopback?
    e, out, err = sh(sid, "python3 -c \"import urllib.request as u;"
                          "u.urlopen('http://127.0.0.1:18990/api/sandbox/status',timeout=5).read()\" 2>&1")
    probe(APP, "cannot reach the daemon's own API on loopback", False, e == 0, out[-100:])

finally:
    sh(sid, "pkill -f 'http.server' 2>/dev/null; true", 10000)
    cleanup(sid)
    for junk in ["/Users/benji/Projects/SemaClaw/SANDBOX_ESCAPE.txt"]:
        if os.path.exists(junk):
            os.remove(junk)
            print("NOTE: removed escape artifact", junk)

dump(os.path.join(os.path.dirname(__file__), "exp1_results.json"))
