#!/usr/bin/env bash
# Where a browser task's time goes: the feed flows ("like this clip", "open
# that clip") through the isolated daemon, decided by laya-browser, with the
# breakdown each task reports in `stats.timing`. Informational — it measures,
# it does not gate.
#
#   SEN_BROWSER_PROFILE=release ./latency.sh            # this branch's runtime
#   SEN_BROWSER_PKG=~/.senclaw/runtimes/sen-browser/0.1.1 ./latency.sh   # a released one
#   LATENCY_OUT=after.json LATENCY_RUNS=5 ./latency.sh
source "$(dirname "$0")/lib.sh"
trap stop EXIT

prepare
start

out="${LATENCY_OUT:-$E2E_DIR/latency.json}"
# LATENCY_ARGS=--cold-only stops after the first task (a cold start each run).
python3 "$HERE/loop-latency.py" "$API" "$FIXTURE" --runs "${LATENCY_RUNS:-3}" --json "$out" ${LATENCY_ARGS:-}
log "written to $out"
