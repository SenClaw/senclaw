#!/usr/bin/env python3
"""How long the browser runtime itself takes to open, observe and act.

Starts `sen-browser serve` by hand (no daemon, no token) with a scratch data
directory, plus the fixture site, and times each HTTP call next to what the
runtime reports about its own work — so transport and waiting are told apart
from input and snapshot. Nothing here touches ~/.senclaw or the live daemon.

usage: runtime-latency.py <sen-browser binary> <scratch dir> [--json out.json] [--sites url ...]
"""
import json
import os
import signal
import socket
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURE_PORT = 28795
RUNTIME_PORT = 28796
FIXTURE = f"http://127.0.0.1:{FIXTURE_PORT}"
RUNTIME = f"http://127.0.0.1:{RUNTIME_PORT}"


def listening(port):
    try:
        socket.create_connection(("127.0.0.1", port), 0.3).close()
        return True
    except OSError:
        return False


def call(method, path, body=None, timeout=60):
    """One runtime call: (wall ms, status, parsed body)."""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(RUNTIME + path, data=data, method=method,
                                 headers={"content-type": "application/json"})
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status, raw = resp.status, resp.read()
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read()
    wall = (time.perf_counter() - started) * 1000
    try:
        return wall, status, json.loads(raw)
    except ValueError:
        return wall, status, {"raw": raw.decode(errors="replace")}


def action(obs, **want):
    """The first observed action whose fields contain every wanted substring."""
    for a in obs.get("actions", []):
        if all(str(v).lower() in str(a.get(k, "")).lower() for k, v in want.items()):
            return a
    raise SystemExit(f"no action matching {want} among {[a.get('label') for a in obs.get('actions', [])]}")


def stats(values):
    values = sorted(values)
    return {"n": len(values), "median": round(statistics.median(values), 1),
            "p90": round(values[min(len(values) - 1, int(len(values) * 0.9))], 1),
            "min": round(values[0], 1), "max": round(values[-1], 1)}


def main():
    binary, scratch = sys.argv[1], os.path.abspath(sys.argv[2])
    args = sys.argv[3:]
    out_path = args[args.index("--json") + 1] if "--json" in args else None
    sites = args[args.index("--sites") + 1:] if "--sites" in args else []
    os.makedirs(scratch, exist_ok=True)
    for port in (FIXTURE_PORT, RUNTIME_PORT):
        if listening(port):
            raise SystemExit(f"port {port} is busy")

    env = {"HOME": scratch, "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
           "SENCLAW_HOME": os.path.join(scratch, ".senclaw"),
           "SENCLAW_RUNTIME_DATA_DIR": os.path.join(scratch, "runtime-data"), "RUST_LOG": os.environ.get("RUNTIME_LOG", "warn")}
    fixture = subprocess.Popen([sys.executable, os.path.join(HERE, "serve.py"), str(FIXTURE_PORT),
                                os.path.join(HERE, "fixtures")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    runtime = subprocess.Popen([binary, "serve", "--host", "127.0.0.1", "--port", str(RUNTIME_PORT)], env=env,
                               stdout=open(os.path.join(scratch, "runtime.log"), "w"), stderr=subprocess.STDOUT)
    report = {}
    try:
        for _ in range(100):
            if listening(FIXTURE_PORT) and listening(RUNTIME_PORT):
                break
            time.sleep(0.1)
        else:
            raise SystemExit("the runtime or the fixture site did not come up")

        # Chrome launch (cold), then the first tab.
        wall, status, session = call("POST", "/v1/sessions", {"driver": "managed", "profile": "latency", "headless": True})
        if status != 200:
            raise SystemExit(f"no session: {session}")
        report["session_open_ms"] = round(wall)
        sid = session["id"]

        def open_page(path, owner="bench"):
            wall, status, r = call("POST", f"/v1/sessions/{sid}/tabs", {"owner": owner, "url": FIXTURE + path})
            if status != 200:
                raise SystemExit(f"open {path}: {r}")
            return wall, r["tab"]["id"], r["observation"]

        def clips_of(obs):
            return sum(1 for a in obs.get("actions", []) if "like clip" in str(a.get("label", "")).lower())

        wall, tab, obs = open_page("/clips.html")
        report["first_tab_open_ms"] = round(wall)

        # Navigation: a plain page, one whose load event waits on a slow
        # subresource, and one whose content arrives after DOMContentLoaded.
        nav = {}
        for name, path in (("plain", "/clips.html"), ("static_no_script", "/help.html"), ("rich", "/travel.html"), ("slow_load_3000", "/clips.html?slow=3000"),
                           ("late_content_600", "/clips.html?late=600"), ("late_600_slow_3000", "/clips.html?late=600&slow=3000")):
            runs = []
            for _ in range(3):
                wall, _, r = call("POST", f"/v1/tabs/{tab}/navigate", {"url": FIXTURE + path})
                runs.append({"wall_ms": round(wall), "clips_seen": clips_of(r), "timing": r.get("timing_ms")})
            nav[name] = runs
        report["navigate"] = nav

        # Observe: the wall time of the call against the snapshot itself.
        call("POST", f"/v1/tabs/{tab}/navigate", {"url": FIXTURE + "/clips.html"})
        walls, snaps = [], []
        for _ in range(20):
            wall, _, r = call("POST", f"/v1/tabs/{tab}/observe", {})
            walls.append(wall)
            snaps.append(r["timing_ms"]["snapshot"])
        report["observe"] = {"wall_ms": stats(walls), "snapshot_ms": stats(snaps)}

        # Act: toggle a like (no navigation), ten times.
        _, _, obs = call("POST", f"/v1/tabs/{tab}/observe", {})
        walls, inputs, settles, snaps = [], [], [], []
        for _ in range(10):
            like = action(obs, kind="click", label="like clip by @mai")
            wall, status, r = call("POST", f"/v1/tabs/{tab}/act", {"observation_id": obs["observation_id"], "action_id": like["id"]})
            if status != 200:
                raise SystemExit(f"act: {r}")
            walls.append(wall)
            inputs.append(r["executed_ms"])
            settles.append(r["settle_ms"])
            obs = r["observation"]
            snaps.append(obs["timing_ms"]["snapshot"])
        report["click_toggle"] = {"wall_ms": stats(walls), "input_ms": stats(inputs), "settle_ms": stats(settles),
                                  "snapshot_ms": stats(snaps)}

        # Act: a click that navigates — to a page that answers at once, then to
        # one that answers late — and the way back.
        for name, path in (("click_navigates", "/clips.html"), ("click_navigates_slow_800", "/clips.html?linkdelay=800")):
            _, _, obs = call("POST", f"/v1/tabs/{tab}/navigate", {"url": FIXTURE + path})
            runs = []
            for _ in range(4):
                link = action(obs, kind="click", label="open clip by @mai")
                wall, _, r = call("POST", f"/v1/tabs/{tab}/act", {"observation_id": obs["observation_id"], "action_id": link["id"]})
                landed = r["observation"]
                run = {"wall_ms": round(wall), "input_ms": r["executed_ms"], "settle_ms": r["settle_ms"],
                       "ready_ms": r.get("ready_ms"), "ready_by": r.get("ready_by"), "landed": landed.get("title")}
                if landed.get("title") != "Clip by @mai":
                    # The observation still shows the page being left: wait the way the loop would.
                    time.sleep(1.2)
                    _, _, landed = call("POST", f"/v1/tabs/{tab}/observe", {})
                back = action(landed, kind="back")
                wall, _, r = call("POST", f"/v1/tabs/{tab}/act", {"observation_id": landed["observation_id"], "action_id": back["id"]})
                obs = r["observation"]
                run.update(back_wall_ms=round(wall), back_ready_by=r.get("ready_by"), back_landed=obs.get("title"))
                runs.append(run)
            report[name] = runs

        # Scroll.
        walls = []
        for _ in range(6):
            down = action(obs, kind="scroll")
            wall, _, r = call("POST", f"/v1/tabs/{tab}/act", {"observation_id": obs["observation_id"], "action_id": down["id"]})
            walls.append(wall)
            obs = r["observation"]
        report["scroll"] = {"wall_ms": stats(walls)}

        # Real pages, when asked: how long opening one takes from here.
        real = {}
        for url in sites:
            runs = []
            for _ in range(2):
                wall, status, r = call("POST", f"/v1/tabs/{tab}/navigate", {"url": url}, timeout=90)
                run = {"wall_ms": round(wall), "status": status, "actions": len(r.get("actions", [])),
                       "text_chars": len(r.get("text", "")), "timing": r.get("timing_ms"), "title": (r.get("title") or "")[:60]}
                # What the page shows a few seconds later: was the first look complete?
                time.sleep(4)
                _, _, later = call("POST", f"/v1/tabs/{tab}/observe", {})
                run.update(actions_later=len(later.get("actions", [])), text_chars_later=len(later.get("text", "")))
                runs.append(run)
            real[url] = runs
        if real:
            report["sites"] = real
    finally:
        for proc in (runtime, fixture):
            proc.send_signal(signal.SIGTERM)
        for proc in (runtime, fixture):
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        # The runtime's own Chrome carries the scratch profile path in its arguments.
        subprocess.run(["pkill", "-TERM", "-f", os.path.join(scratch, "runtime-data")], check=False)

    text = json.dumps(report, indent=2)
    if out_path:
        with open(out_path, "w") as f:
            f.write(text)
    print(text)


if __name__ == "__main__":
    main()
