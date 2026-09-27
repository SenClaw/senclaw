#!/usr/bin/env python3
"""Experiment 2 — a REAL Space App (Express + sqlite3 native module) sandboxed.

Subject: apps/test-manager from this repo — Node 24, Express 5, sqlite3, multer.

Two setups, because the obvious one breaks the app:
  A) app mounted READ-ONLY  → sqlite cannot create its DB file next to db.js
  B) app copied into the sandbox workspace (writable), data mounted read-only

Setup B is the pattern to document. Both are measured rather than assumed.
"""
import json
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from harness import (api, create, sh, probe, host_get, wait_port, cleanup,  # noqa: E402
                     dump)

APP = "test-manager"
REPO_APP = "/Users/benji/Projects/SemaClaw/apps/test-manager"
HOME = os.path.expanduser("~")
DATA = "/private/tmp/claude-501/-Users-benji-Projects-SemaClaw/9a506870-d4fc-428c-b738-c057abe289f0/scratchpad/exp/tm-data"
PORT = 4108

os.makedirs(DATA, exist_ok=True)
open(os.path.join(DATA, "cases.csv"), "w").write("id,title\n1,login works\n")

# ── Setup A: the naive one — mount the app read-only ───────────────────────
sid_a = create("exp2-ro", network=False, fsMode="strict", timeoutMs=120000,
               memoryMb=2048, cpus=2)
try:
    api(f"/sandboxes/{sid_a}/mounts", "POST", {"source": REPO_APP, "readOnly": True})
    api(f"/sandboxes/{sid_a}/ports", "POST", {"listen": [PORT], "connect": []})
    e, out, err = sh(sid_a, f"cd '{REPO_APP}' && node server.js > /dev/null 2>start.err; "
                            f"echo exit=$?; head -c 200 start.err", 40000)
    blob = (out or "") + (err or "")
    probe(APP, "A: read-only app dir — sqlite refuses to open its DB", False,
          "exit=0" in blob, blob.replace("\n", " ")[-140:])
finally:
    cleanup(sid_a)

# ── Setup B: the working pattern — app inside the workspace ────────────────
sid_b = create("exp2-app", network=False, fsMode="strict", timeoutMs=180000,
               memoryMb=2048, cpus=2)
info = api(f"/sandboxes/{sid_b}")
workdir = info["workdir"]
print("workdir:", workdir)
try:
    dest = os.path.join(workdir, "test-manager")
    subprocess.run(["cp", "-R", REPO_APP, dest], check=True)
    # A fresh DB, so the copy does not inherit repo state.
    for junk in ["test-manager.sqlite", "test-manager.zip"]:
        p = os.path.join(dest, junk)
        if os.path.exists(p):
            os.remove(p)

    api(f"/sandboxes/{sid_b}/mounts", "POST", {"source": DATA, "readOnly": True})
    api(f"/sandboxes/{sid_b}/ports", "POST", {"listen": [PORT], "connect": []})

    # `( cmd & )` — a plain `&` keeps the exec blocked until its deadline and
    # the group kill then takes the server down with it (measured: node blocks,
    # python happens not to). The subshell detaches, so exec returns at once.
    sh(sid_b, "cd test-manager && (node server.js < /dev/null > server.log 2>&1 &) ; echo up", 30000)
    time.sleep(5)
    up = wait_port(PORT)
    st, body = host_get(f"http://127.0.0.1:{PORT}/api/health")
    if st is None:
        st, body = host_get(f"http://127.0.0.1:{PORT}/")
    _, log = sh(sid_b, "tail -5 test-manager/server.log", 15000)[1:]
    probe(APP, "B: the real Express+sqlite app boots and answers on its port",
          True, up and st is not None, f"HTTP {st} · log: {str(log)[:80]}")

    e, out, _ = sh(sid_b, "ls -la test-manager/test-manager.sqlite 2>&1 | tail -1")
    probe(APP, "B: sqlite created its database inside the workspace", True,
          "test-manager.sqlite" in out and "No such" not in out, out.strip()[:90])

    e, out, _ = sh(sid_b, f"cat '{DATA}/cases.csv'")
    probe(APP, "B: reads the data folder it was mounted", True,
          e == 0 and "login works" in out, out.strip()[:60])

    e, out, err = sh(sid_b, f"echo x > '{DATA}/cases.csv'")
    poisoned = "login works" not in open(os.path.join(DATA, "cases.csv")).read()
    probe(APP, "B: cannot modify the read-only data mount", False, poisoned,
          (err or out)[-90:])

    e, out, _ = sh(sid_b, f"ls {HOME}/Documents 2>&1 | head -2; ls {HOME}/.senclaw 2>&1 | head -2")
    denied = "Operation not permitted" in out or "No such file" in out
    probe(APP, "B: cannot list the user's home folders", False, not denied, out.strip()[:90])

    # Node is a different runtime from Python — check the network rules bind it too.
    e, out, err = sh(sid_b, "node -e \"require('net').connect(443,'1.1.1.1')"
                            ".on('error',e=>{console.error(e.code);process.exit(3)})"
                            ".on('connect',()=>process.exit(0))\" 2>&1")
    probe(APP, "B: Node cannot open an outbound connection either", False, e == 0,
          (out or err).strip()[-90:])

    e, out, err = sh(sid_b, "node -e \"require('net').connect(18990,'127.0.0.1')"
                            ".on('error',e=>{console.error(e.code);process.exit(3)})"
                            ".on('connect',()=>process.exit(0))\" 2>&1")
    probe(APP, "B: Node cannot reach the daemon API on loopback", False, e == 0,
          (out or err).strip()[-90:])
finally:
    sh(sid_b, "pkill -f 'node server.js' 2>/dev/null; true", 15000)
    cleanup(sid_b)

dump(os.path.join(os.path.dirname(__file__), "exp2_results.json"))
