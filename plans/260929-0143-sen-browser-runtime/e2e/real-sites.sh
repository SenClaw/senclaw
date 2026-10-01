#!/usr/bin/env bash
# Real sites, through the loop, with one laya-browser checkpoint — a smoke run, not a
# check: sites change and block headless Chrome. Isolated like the rest (scratch HOME,
# ports 28788/28789). One line per step, then the request format the loop used.
#
#   E2E_DIR=<scratch> LAYA_BROWSER_SRC=<checkpoint dir> bash real-sites.sh
source "$(dirname "$0")/lib.sh"
trap stop EXIT

prepare
start

run_task() { # run_task JSON
  local out
  out=$(API_TIMEOUT=600 api POST /api/browser-agent/tasks "$1")
  python3 - "$out" <<'PY'
import json, sys
o = json.loads(sys.argv[1])
s = o.get("stats", {})
print(f"  status={o.get('status')} steps={s.get('steps')} decisions={s.get('decisions')} llm_calls={s.get('llm_calls')} "
      f"elapsed={s.get('elapsed_ms')}ms timing={s.get('timing')}")
for st in o.get("steps", []):
    conf = st.get("confidence")
    print(f"    {st['step']:>2} {st['operation']:<10} by={st['by']:<4} conf={conf if conf is None else round(conf, 2)} "
          f"{st['decision_ms']:>5}ms  {st['label'][:60]!r} -> {st['outcome']}")
print(f"  message: {o.get('message', '')[:200]}")
for n in o.get("notes", []):
    print(f"  note: {n}")
PY
}

for task in \
  '{"goal": "Search for tokio rust and open the result from tokio.rs.", "url": "https://duckduckgo.com/", "done_criteria": ["The page is on the tokio.rs website"], "max_steps": 8, "browser": "managed"}' \
  '{"goal": "Search Wikipedia for Hanoi and open the article about the city.", "url": "https://en.wikipedia.org/wiki/Main_Page", "done_criteria": ["The page is the Wikipedia article titled \"Hanoi\""], "max_steps": 8, "browser": "managed"}' \
  '{"goal": "From this page, open the Wikipedia article about the Mekong river.", "url": "https://en.wikipedia.org/wiki/Vietnam", "done_criteria": ["The page is the Wikipedia article titled \"Mekong\""], "max_steps": 8, "browser": "managed"}'
do
  echo "== $(python3 -c 'import json,sys; t=json.loads(sys.argv[1]); print(t["url"], "|", t["goal"])' "$task")"
  run_task "$task"
done
grep -h "asked in request format" "$E2E_DIR/daemon.log" | tail -1
