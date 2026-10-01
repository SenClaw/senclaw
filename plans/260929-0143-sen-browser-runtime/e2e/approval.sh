#!/usr/bin/env bash
# E3 — a purchase waits for the person. A browser task in SenClaw's own Chrome
# reaches "Place order"; the engine must pause there (needs_approval) without
# clicking, list the action at GET /api/browser-agent/approvals (what Settings
# → Browser shows), and place the order only after the person approves it
# there. The fixture server's log is the witness: the order page is fetched
# only after the approval. Prints "E3 PASS".
source "$(dirname "$0")/lib.sh"
trap stop EXIT

prepare
start

args=$(python3 - "$FIXTURE" <<'PY'
import json, sys
print(json.dumps({"goal": "Buy the blue mug in my cart", "url": sys.argv[1] + "/checkout.html",
                  "done_criteria": ['The page shows "order is placed"'], "max_steps": 6, "browser": "managed"}))
PY
)
log "browser_task via MCP (e3-order): $args"
python3 "$HERE/mcp_task.py" "$SENCLAW_BIN" "$SCRATCH_HOME" "$API" browser_task "$args" >"$E2E_DIR/e3-task.json"
grep -c "GET /order-placed.html" "$E2E_DIR/fixture.log" >"$E2E_DIR/e3-before.txt" || true
api GET /api/browser-agent/approvals >"$E2E_DIR/e3-approvals.json"

approval_id=$(python3 - "$E2E_DIR" <<'PY'
import json, sys
d = sys.argv[1]
task = json.load(open(f"{d}/e3-task.json"))
print(f"  task: status={task['status']} message={task.get('message')!r}", file=sys.stderr)
if task["status"] != "needs_approval":
    sys.exit(f"E3 FAIL: the task did not pause: {task['status']}: {task.get('message')}")
pending = task.get("pending") or {}
if "place order" not in (pending.get("action") or "").lower():
    sys.exit(f"E3 FAIL: paused on {pending.get('action')!r}, not the order button")
if open(f"{d}/e3-before.txt").read().strip() not in ("", "0"):
    sys.exit("E3 FAIL: the order page was fetched before anyone approved")
listed = json.load(open(f"{d}/e3-approvals.json")).get("approvals", [])
if not any(a.get("approval_id") == pending.get("approval_id") for a in listed):
    sys.exit(f"E3 FAIL: the approval is not listed for the settings screens: {listed}")
print(pending["approval_id"])
PY
)
log "approving $approval_id as the person would in Settings → Browser"
api POST "/api/browser-agent/approvals/$approval_id" '{"approve":true}' >"$E2E_DIR/e3-approved.json"
api GET /api/browser-agent/approvals >"$E2E_DIR/e3-approvals-after.json"

python3 - "$E2E_DIR" "$approval_id" <<'PY'
import json, sys
from urllib.parse import urlparse
d, approval_id = sys.argv[1], sys.argv[2]
out = json.load(open(f"{d}/e3-approved.json"))
print(f"  after approval: status={out.get('status')} url={out.get('url')} message={out.get('message')!r}", file=sys.stderr)
fetched = "GET /order-placed.html" in open(f"{d}/fixture.log").read()
if not fetched or urlparse(out.get("url") or "").path != "/order-placed.html":
    sys.exit(f"E3 FAIL: the order was not placed after approval: {out.get('status')}: {out.get('message')}")
left = json.load(open(f"{d}/e3-approvals-after.json")).get("approvals", [])
if any(a.get("approval_id") == approval_id for a in left):
    sys.exit("E3 FAIL: the answered approval is still listed")
print("E3 PASS")
PY
