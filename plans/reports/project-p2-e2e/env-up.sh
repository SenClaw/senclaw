#!/bin/zsh
# usage: env-up.sh <label> <senclaw binary>   → starts the P2 mock LLM + an isolated daemon (own HOME, SENCLAW_HOME, cwd, ports)
# Never uses 18788/18789. Needs P0_RUNS / P0_UI / P0_WS / P0_MOCK set to free ports (defaults 38788/38789/38931).
set -u
LABEL=$1; BIN=$2; shift 2
HERE=$(cd "$(dirname "$0")" && pwd)
SP=${P0_RUNS:-/tmp/senclaw-p0-runs}
mkdir -p $SP
S=$SP/run-$LABEL
UI=${P0_UI:-38788}; WS=${P0_WS:-38789}; MOCK=${P0_MOCK:-38931}
for p in $UI $WS $MOCK; do
  if [ "$p" = 18788 ] || [ "$p" = 18789 ]; then echo "refusing the real daemon port $p"; exit 1; fi
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/proj/A $S/proj/B $S/proj/C $S/log $S/mock
cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"mock","label":"mock","provider":"custom","baseURL":"http://127.0.0.1:$MOCK/v1","modelName":"mock-model","adapt":"openai","apiKey":"local","maxTokens":2048,"contextLength":32000}],
 "activeLlmConfigId":"mock","activeQuickLlmConfigId":"mock","activeCognitiveLlmConfigId":"mock"}
JSON
for d in A B C; do echo "marker $d" > $S/proj/$d/marker-$d.txt; done
cd $S/mock && nohup node $HERE/mock-llm-p2.mjs --port $MOCK --out $S/mock > $S/log/mock.log 2>&1 &
echo $! > $S/mock.pid
cd $S/root && env HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS \
  SENCLAW_COGNITIVE_REFLECT_WINDOW_IDLE_MS=1500 SENCLAW_COGNITIVE_REFLECT_COOLDOWN_MS=0 RUST_LOG=info "$@" \
  nohup $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do
  curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break
  sleep 1
done
echo "S=$S daemon pid $(cat $S/daemon.pid) mock pid $(cat $S/mock.pid)"
