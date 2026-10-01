#!/usr/bin/env bash
# E2 — the same browser tasks in "the person's Chrome": a separate Chrome for
# Testing with the SenClaw extension v0.2 loaded, connected to the isolated
# daemon at /browser/ext, paired through the REST approval (what "pair
# approve <CODE>" does in chat), then driven through chrome.debugger by the
# runtime's extension driver. Prints "E2 PASS".
source "$(dirname "$0")/lib.sh"

EXT_REPO="$ROOT/senclaw-extension"
EXT_DIR="$E2E_DIR/extension"
CFT="${CFT:-$(ls -d "$HOME"/Library/Caches/ms-playwright/chromium-*/chrome-mac*/"Google Chrome for Testing.app" 2>/dev/null | tail -1)/Contents/MacOS/Google Chrome for Testing}"
CHROME_PID=""

stop_all() {
  if [[ -n "$CHROME_PID" ]]; then kill -TERM "$CHROME_PID" 2>/dev/null || true; fi
  pkill -TERM -f "$E2E_DIR/ext-profile" 2>/dev/null || true
  stop
}
trap stop_all EXIT

[[ -x "$CFT" ]] || die "Chrome for Testing not found (set CFT=…)"

# A test build that dials the isolated daemon from its first connection.
(cd "$EXT_REPO" && WXT_SENCLAW_WS_PORT=$WS_PORT npx wxt build >/dev/null)
rm -rf "$EXT_DIR" && cp -R "$EXT_REPO/dist/chrome-mv3" "$EXT_DIR"
(cd "$EXT_REPO" && npm run build >/dev/null)   # leave the repo's own build as it was
grep -q "$WS_PORT" "$EXT_DIR/background.js" || die "the test build does not point at port $WS_PORT"

prepare
start

rm -rf "$E2E_DIR/ext-profile"
"$CFT" --user-data-dir="$E2E_DIR/ext-profile" --headless=new --no-first-run --no-default-browser-check \
  --use-mock-keychain --disable-extensions-except="$EXT_DIR" --load-extension="$EXT_DIR" about:blank \
  >"$E2E_DIR/cft.log" 2>&1 &
CHROME_PID=$!

# The extension says hello and waits for the person with a code.
code=""
for _ in $(seq 1 60); do
  code=$(api GET /api/browser-agent/extension | python3 -c 'import json,sys; p=json.load(sys.stdin).get("pending",[]); print(p[0]["code"] if p else "")')
  [[ -n "$code" ]] && break
  sleep 0.5
done
[[ -n "$code" ]] || die "the extension never asked to pair ($(api GET /api/browser-agent/extension))"
log "pairing code $code — approving it"
api POST "/api/browser-agent/extension/pairings/$code/approve" '{}' >/dev/null
for _ in $(seq 1 20); do
  connected=$(api GET /api/browser-agent/extension | python3 -c 'import json,sys; print(bool(json.load(sys.stdin).get("connected")))')
  [[ "$connected" == True ]] && break
  sleep 0.25
done
[[ "$connected" == True ]] || die "the extension did not connect after approval"
log "extension paired and connected"

task_json() { # task_json GOAL START-PATH CRITERION...
  python3 - "$FIXTURE" "$@" <<'PY'
import json, sys
fixture, goal, path, *criteria = sys.argv[1:]
print(json.dumps({"goal": goal, "url": fixture + path, "done_criteria": criteria, "max_steps": 8, "browser": "extension"}))
PY
}

run_task() { # run_task NAME JSON-ARGS
  log "browser_task via MCP ($1): $2"
  python3 "$HERE/mcp_task.py" "$SENCLAW_BIN" "$SCRATCH_HOME" "$API" browser_task "$2" >"$E2E_DIR/$1-result.json"
  python3 - "$E2E_DIR/$1-result.json" <<'PY'
import json, sys
out = json.load(open(sys.argv[1]))
for s in out.get("steps", []):
    conf = s.get("confidence")
    conf = f"{conf:.2f}" if isinstance(conf, (int, float)) else "-"
    print(f"  step {s['step']}: {s['operation']:<10} {s.get('label','')[:40]:<40} by={s['by']:<4} conf={conf} -> {s['outcome']}")
print(f"  status={out['status']} driver={out.get('driver')} url={out.get('url')} message={out.get('message')!r}")
PY
}

run_task e2-help "$(task_json "Open the Help page" / 'The page shows "Type a destination city"')"
run_task e2-deals "$(task_json "Open the Deals page" / 'The page shows "Deals from Zurich"')"

python3 - "$E2E_DIR" <<'PY'
import json, sys
from urllib.parse import urlparse
d = sys.argv[1]
for name, path in (("e2-help", "/help.html"), ("e2-deals", "/deals.html")):
    out = json.load(open(f"{d}/{name}-result.json"))
    ok = (out["status"] == "done" and out.get("driver") == "extension"
          and urlparse(out.get("url") or "").path == path
          and out.get("evidence") and all(e.get("verdict") for e in out["evidence"]))
    if not ok:
        sys.exit(f"E2 FAIL: {name}: {out['status']} via {out.get('driver')}: {out.get('message')} ({out.get('url')})")
print("E2 PASS")
PY
