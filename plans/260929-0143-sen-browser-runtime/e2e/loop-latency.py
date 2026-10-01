#!/usr/bin/env python3
"""Where a browser task's time goes, measured through the daemon's own API.

Runs the feed flows a chat would ask for ("like this clip", "open that clip")
as whole tasks — the decision loop — and as the step-by-step tool sequence an
agent drives by hand, against the isolated daemon `lib.sh` started. Prints a
table per scenario from the task's own `stats.timing`.

usage: loop-latency.py <api url> <fixture url> [--runs N] [--json out.json] [--cold-only]
"""
import json
import statistics
import sys
import time
import urllib.error
import urllib.request

API, FIXTURE = sys.argv[1].rstrip("/"), sys.argv[2].rstrip("/")
ARGS = sys.argv[3:]
RUNS = int(ARGS[ARGS.index("--runs") + 1]) if "--runs" in ARGS else 3
OUT = ARGS[ARGS.index("--json") + 1] if "--json" in ARGS else None
CHAT = "latency-chat"


def call(method, path, body=None, timeout=900):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(API + path, data=data, method=method, headers={"content-type": "application/json"})
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
    except urllib.error.HTTPError as e:
        raw = e.read()
    wall = (time.perf_counter() - started) * 1000
    try:
        return wall, json.loads(raw)
    except ValueError:
        return wall, {"error": raw.decode(errors="replace")}


def task(goal, path, criteria=None, question=None, max_steps=8):
    body = {"goal": goal, "url": FIXTURE + path, "max_steps": max_steps, "browser": "managed", "chat_jid": CHAT}
    if criteria is not None:
        body["done_criteria"] = criteria
    if question:
        body["question"] = question
    wall, out = call("POST", "/api/browser-agent/tasks", body)
    stats = out.get("stats", {})
    return {
        "wall_ms": round(wall), "status": out.get("status"), "message": out.get("message"),
        "steps": [{"op": s["operation"], "label": s.get("label", "")[:40], "by": s["by"], "band": s.get("band"),
                   "decision_ms": s.get("decision_ms"), "act_ms": s.get("act_ms"), "outcome": s["outcome"]} for s in out.get("steps", [])],
        "counts": {k: stats.get(k) for k in ("steps", "decisions", "llm_calls", "fallbacks", "stale")},
        "elapsed_ms": stats.get("elapsed_ms"), "timing": stats.get("timing", {}),
    }


def element(page, *words):
    for e in page.get("elements", []):
        label = str(e.get("label", "")).lower()
        if all(w.lower() in label for w in words):
            return e["index"]
    raise SystemExit(f"no element {words} in {[e.get('label') for e in page.get('elements', [])]}")


def manual():
    """The same flow, one tool call at a time (what a chat model drives by hand)."""
    steps = []
    wall, opened = call("POST", "/api/browser-agent/open", {"url": FIXTURE + "/clips.html?reset=1", "browser": "managed", "chat_jid": CHAT})
    page = opened.get("page") or {}
    steps.append(("open feed", wall, len(json.dumps(opened))))
    wall, done = call("POST", "/api/browser-agent/do", {"observation_id": page.get("observation_id"), "operation": "CLICK",
                                                         "target": element(page, "like clip", "@mai"), "browser": "managed", "chat_jid": CHAT})
    page = done.get("page") or {}
    steps.append(("like", wall, len(json.dumps(done))))
    wall, done = call("POST", "/api/browser-agent/do", {"observation_id": page.get("observation_id"), "operation": "CLICK",
                                                         "target": element(page, "open clip", "@mai"), "browser": "managed", "chat_jid": CHAT})
    steps.append(("open clip", wall, len(json.dumps(done))))
    wall, read = call("POST", "/api/browser-agent/read", {"browser": "managed", "chat_jid": CHAT})
    steps.append(("read", wall, len(json.dumps(read))))
    return [{"call": name, "wall_ms": round(w), "result_chars": chars} for name, w, chars in steps]


def median(values):
    values = [v for v in values if isinstance(v, (int, float))]
    return round(statistics.median(values)) if values else None


SCENARIOS = [
    ("like", "Like the clip by @mai", "/clips.html?reset=1", ['The page shows "You liked the clip by @mai"']),
    ("open", "Open the clip by @mai", "/clips.html?reset=1", ['The page shows "Now playing"']),
    ("open+like", "Open the clip by @mai and like it", "/clips.html?reset=1",
     ['The page shows "Now playing"', 'The page shows "You liked the clip by @mai"']),
    # No criteria given: the loop has an LLM write them before it can accept DONE.
    ("like, no criteria", "Like the clip by @mai", "/clips.html?reset=1", None),
]

report = {"runs": RUNS, "tasks": {}, "manual": []}

# The first task pays for every cold start: the runtimes, Chrome, the models.
cold = task("Open the Help page", "/", ['The page shows "Type a destination city"'])
report["cold_start"] = cold
print(f"cold start: wall={cold['wall_ms']} ms status={cold['status']} timing={cold['timing']}")
if "--cold-only" in ARGS:
    sys.exit(0)

for name, goal, path, criteria in SCENARIOS:
    runs = [task(goal, path, criteria) for _ in range(RUNS)]
    report["tasks"][name] = runs
    keys = ("open", "decide", "llm", "risk", "text", "act", "page", "verify", "answer")
    med = {k: median([r["timing"].get(k) for r in runs]) for k in keys}
    print(f"\n== {name}: {goal!r}")
    for r in runs:
        path_taken = " → ".join(f"{s['op']}[{s['by']}]" for s in r["steps"])
        print(f"   wall={r['wall_ms']:>6} ms  status={r['status']:<14} {r['counts']}  {path_taken}")
    print(f"   median wall={median([r['wall_ms'] for r in runs])} ms  loop={median([r['elapsed_ms'] for r in runs])} ms  " +
          "  ".join(f"{k}={v}" for k, v in med.items()))

for _ in range(RUNS):
    report["manual"].append(manual())
print("\n== by hand: open → like → open clip → read (tool time only, no chat-model turns)")
for i, name in enumerate(["open feed", "like", "open clip", "read"]):
    walls = [run[i]["wall_ms"] for run in report["manual"]]
    chars = [run[i]["result_chars"] for run in report["manual"]]
    print(f"   {name:<10} median {median(walls):>5} ms   result {median(chars):>6} chars")

if OUT:
    with open(OUT, "w") as f:
        json.dump(report, f, indent=2)
