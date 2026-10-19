#!/bin/zsh
# usage: env-up.sh <label> <senclaw binary>   → starts the P3b mock LLM + an isolated daemon (own HOME, SENCLAW_HOME, cwd, ports)
# Never uses 18788/18789/18790; the daemon runs under Seatbelt with loopback connects to those ports DENIED, so a stray
# call to the real daemon fails instead of reaching it. Needs P0_RUNS / P0_UI / P0_WS / P0_MOCK set to free ports.
set -u
LABEL=$1; BIN=$2; shift 2
HERE=$(cd "$(dirname "$0")" && pwd)
SP=${P0_RUNS:-/tmp/senclaw-p0-runs}
mkdir -p $SP
S=$SP/run-$LABEL
UI=${P0_UI:-38788}; WS=${P0_WS:-38789}; MOCK=${P0_MOCK:-38931}
for p in $UI $WS $MOCK; do
  if [ "$p" = 18788 ] || [ "$p" = 18789 ] || [ "$p" = 18790 ]; then echo "refusing the real daemon port $p"; exit 1; fi
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/log $S/mock
for d in A B C D E; do mkdir -p $S/proj/$d; echo "marker $d" > $S/proj/$d/marker-$d.txt; done
cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"mock","label":"mock","provider":"custom","baseURL":"http://127.0.0.1:$MOCK/v1","modelName":"mock-model","adapt":"openai","apiKey":"local","maxTokens":2048,"contextLength":32000}],
 "activeLlmConfigId":"mock","activeQuickLlmConfigId":"mock","activeCognitiveLlmConfigId":"mock"}
JSON
cd $S/mock && nohup node $HERE/mock-llm-p3b.mjs --port $MOCK --out $S/mock > $S/log/mock.log 2>&1 &
echo $! > $S/mock.pid
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
# env -u: an exported variable in this shell must not mask what the daemon is told below.
# SENCLAW_BASE_URL/SENCLAW_APP_TOKEN_MODE: kanban's AI routes call the SDK client, whose default base URL is the real daemon.
cd $S/root && env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE -u SENCLAW_HOME \
  HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS \
  SENCLAW_BASE_URL=http://127.0.0.1:$UI SENCLAW_APP_TOKEN_MODE=off SENCLAW_DISPATCH_INTERVAL_SECS=3 SENCLAW_DISPATCH_CARD_FACTS=true \
  RUST_LOG=info "$@" \
  nohup sandbox-exec -p "$PROFILE" $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do
  curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break
  sleep 1
done
echo "S=$S daemon pid $(cat $S/daemon.pid) mock pid $(cat $S/mock.pid)"
grep -c "MCP-external HTTP connect" $S/log/daemon.log | sed 's/^/MCP-external HTTP connect lines: /'
