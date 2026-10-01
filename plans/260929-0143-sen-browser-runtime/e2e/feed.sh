#!/usr/bin/env bash
# E4 — a feed the way short-video sites build one, through the agent's real
# surface (MCP core-server → /api/browser-agent/* → sen-browser → Chrome):
#
#   1. "Like the clip" is one browser_task call: one click, verified on the
#      page, with the time accounted for and nothing to say about a slow run.
#   2. The like is a toggle: once it is on, a second task must not click it off.
#   3. A page whose load event is five seconds away is opened in well under
#      that, and one whose clips arrive from its own script shows them at once.
#   4. browser_read with a url opens and reads in a single call.
#   5. The browser skill the agent loads is the text written for this engine.
#
# Prints "E4 PASS".
source "$(dirname "$0")/lib.sh"
trap stop EXIT

prepare
start

mcp() { # mcp TOOL JSON-ARGS → the tool's JSON result
  python3 "$HERE/mcp_task.py" "$SENCLAW_BIN" "$SCRATCH_HOME" "$API" "$1" "$2"
}

mcp browser_task "$(python3 - "$FIXTURE" <<'PY'
import json, sys
print(json.dumps({"goal": "Like the clip by @mai", "url": sys.argv[1] + "/clips.html?reset=1",
                  "done_criteria": ['The page shows "You liked the clip by @mai"'], "max_steps": 4, "browser": "managed"}))
PY
)" >"$E2E_DIR/e4-like.json"

# The clip is liked now; asked again, the loop has nothing to click.
mcp browser_task "$(python3 - "$FIXTURE" <<'PY'
import json, sys
print(json.dumps({"goal": "Like the clip by @mai", "url": sys.argv[1] + "/clips.html",
                  "done_criteria": ['The page shows "You liked the clip by @mai"'], "max_steps": 4, "browser": "managed"}))
PY
)" >"$E2E_DIR/e4-again.json"

now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }
opened=$(now_ms)
mcp browser_open "{\"url\": \"$FIXTURE/clips.html?slow=5000\", \"browser\": \"managed\"}" >"$E2E_DIR/e4-slow.json"
echo $(( $(now_ms) - opened )) >"$E2E_DIR/e4-slow-ms.txt"
mcp browser_open "{\"url\": \"$FIXTURE/clips.html?late=500\", \"browser\": \"managed\"}" >"$E2E_DIR/e4-late.json"
mcp browser_read "{\"url\": \"$FIXTURE/clip.html?id=2\", \"browser\": \"managed\"}" >"$E2E_DIR/e4-read.json"
api GET /api/skills >"$E2E_DIR/e4-skills.json"
api GET /api/browser-agent/settings >"$E2E_DIR/e4-settings.json"

python3 - "$E2E_DIR" <<'PY'
import json, sys
d = sys.argv[1]
load = lambda name: json.load(open(f"{d}/{name}.json"))
fail = lambda why: sys.exit(f"E4 FAIL: {why}")

like = load("e4-like")
timing = like.get("stats", {}).get("timing", {})
print(f"  like: status={like.get('status')} steps={[s['operation'] + ' ' + s['label'] for s in like.get('steps', [])]} "
      f"elapsed={like.get('stats', {}).get('elapsed_ms')} ms timing={timing}", file=sys.stderr)
if like.get("status") != "done" or not all(e.get("verdict") for e in like.get("evidence", [{}])):
    fail(f"the like task ended {like.get('status')}: {like.get('message')}")
steps = like.get("steps", [])
if len(steps) != 1 or steps[0]["operation"] != "CLICK" or "like clip by @mai" not in steps[0]["label"].lower():
    fail(f"expected one click on the like button, got {steps}")
if like.get("notes"):
    fail(f"a run on its decision model has nothing to note: {like['notes']}")
for part in ("open", "decide", "act", "verify"):
    if not isinstance(timing.get(part), int):
        fail(f"no `{part}` in stats.timing: {timing}")
if like["stats"]["llm_calls"] != 0:
    fail(f"criteria were given, the decision model was sure: no LLM call was due ({like['stats']})")

again = load("e4-again")
print(f"  again: status={again.get('status')} steps={[s['operation'] + ' ' + s['label'] for s in again.get('steps', [])]}", file=sys.stderr)
if again.get("status") != "done" or again.get("steps"):
    fail(f"a clip already liked was clicked again (or the task did not end done): {again.get('status')} {again.get('steps')}")

slow, late = load("e4-slow"), load("e4-late")
labels = lambda out: [e.get("label", "") for e in (out.get("page") or {}).get("elements", [])]
slow_ms = int(open(f"{d}/e4-slow-ms.txt").read())
print(f"  a feed whose load event is 5 s away: opened in {slow_ms} ms (the tool call, MCP start included)", file=sys.stderr)
if not any("Like clip by @mai" in l for l in labels(slow)):
    fail(f"the slow-loading feed came back without its clip: {labels(slow)}")
if slow_ms >= 4000:
    fail(f"opening waited {slow_ms} ms for a load event the page did not need")
if not any("Like clip by @mai" in l for l in labels(late)):
    fail(f"the feed rendered by its own script came back as a shell: {labels(late)}")
toggles = [e for e in (slow.get("page") or {}).get("elements", []) if "Like clip by @mai" in e.get("label", "")]
if toggles[0].get("checked") != "true":
    fail(f"the like's state is not reported: {toggles[0]}")

read = load("e4-read")
if "Now playing: Street coffee, slow pour" not in read.get("text", ""):
    fail(f"browser_read with a url did not read that page: {str(read)[:200]}")

skills = {s["name"]: s for s in load("e4-skills") if isinstance(s, dict)} if isinstance(load("e4-skills"), list) else \
         {s["name"]: s for s in load("e4-skills").get("skills", [])}
for name in ("agent-browser", "web-research"):
    version = (skills.get(name) or {}).get("version") or ""
    if not version.startswith("2."):
        fail(f"{name} is not the text for engine v2 (version {version!r})")

model = load("e4-settings").get("decisionModel") or {}
if model != {"id": "laya-browser", "needed": True, "installed": True}:
    fail(f"decisionModel is reported as {model}")
print("E4 PASS")
PY
