#!/bin/zsh
# usage: env-up.sh <label> <senclaw binary>   → starts mock LLM + isolated daemon, prints paths
set -u
LABEL=$1; BIN=$2; shift 2   # remaining args: extra KEY=VAL for the daemon
HERE=$(cd "$(dirname "$0")" && pwd)
SP=${P0_RUNS:-/tmp/senclaw-p0-runs}
mkdir -p $SP
S=$SP/run-$LABEL
UI=${P0_UI:-38788}; WS=${P0_WS:-38789}; MOCK=${P0_MOCK:-38931}
for p in $UI $WS $MOCK; do
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/proj/A $S/proj/B $S/log $S/mock
cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"mock","label":"mock","provider":"custom","baseURL":"http://127.0.0.1:$MOCK/v1","modelName":"mock-model","adapt":"openai","apiKey":"local","maxTokens":2048,"contextLength":32000}],
 "activeLlmConfigId":"mock","activeQuickLlmConfigId":"mock","activeCognitiveLlmConfigId":"mock"}
JSON
echo "marker A" > $S/proj/A/marker-A.txt; echo "marker B" > $S/proj/B/marker-B.txt
printf '{"text":"seed-from-A one","label":"a"}\n{"text":"seed-from-A two","label":"a"}\n' > $S/proj/A/seeds.jsonl
printf '{"text":"seed-from-B one","label":"b"}\n{"text":"seed-from-B two","label":"b"}\n' > $S/proj/B/seeds.jsonl
mkdir -p $S/home/senclaw/virtual-agents $S/home/senclaw/workflows
cat > $S/home/senclaw/virtual-agents/p0-worker.md <<'PERSONA'
---
name: p0-worker
description: Spike worker persona (executes whatever script the test hands it)
max_concurrent: 3
tools: Bash, Read, Write
---

PERSONA-SYSTEM-MARKER-7f3a: you are the P0 spike worker. Do exactly what the task says.
PERSONA
cd $S/mock && nohup node $HERE/mock-llm.mjs --port $MOCK --out $S/mock > $S/log/mock.log 2>&1 &
echo $! > $S/mock.pid
cd $S/root && env HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS \
  SENCLAW_COGNITIVE_REFLECT_WINDOW_IDLE_MS=6000 RUST_LOG=info "$@" nohup $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do
  curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break
  sleep 1
done
echo "S=$S daemon pid $(cat $S/daemon.pid) mock pid $(cat $S/mock.pid)"
grep -c "MCP-external HTTP connect" $S/log/daemon.log | sed 's/^/MCP-external HTTP connect lines: /'
