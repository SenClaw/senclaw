#!/bin/zsh
# usage: up.sh <label> <senclaw binary> [guard]   → isolated daemon + scripted mock LLM
# `guard` runs the daemon under sandbox-exec denying loopback connects to the real daemon's ports
# (18788/18789/18790). Leave it off when the agent's Bash must itself use the Seatbelt sandbox
# (sandbox-exec cannot nest).
set -u
LABEL=$1; BIN=$2; GUARD=${3:-}
HERE=$(cd "$(dirname "$0")" && pwd)
SP=${BG_RUNS:?set BG_RUNS to a scratch directory}
S=$SP/run-$LABEL
UI=${BG_UI:-38788}; WS=${BG_WS:-38789}; MOCK=${BG_MOCK:-38931}
for p in $UI $WS $MOCK; do
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
if [ ! -d $S/home ]; then
  mkdir -p $S/home/.senclaw $S/root $S/work $S/log $S/mock
  cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"mock","label":"mock","provider":"custom","baseURL":"http://127.0.0.1:$MOCK/v1","modelName":"mock-model","adapt":"openai","apiKey":"local","maxTokens":2048,"contextLength":32000}],
 "activeLlmConfigId":"mock","activeQuickLlmConfigId":"mock","activeCognitiveLlmConfigId":"mock"}
JSON
fi
cp "$BIN" $S/senclaw-bin
cd $S/mock && nohup node $HERE/mock-llm.mjs --port $MOCK --out $S/mock > $S/log/mock.log 2>&1 &
echo $! > $S/mock.pid
cd $S/root
if [ "$GUARD" = guard ]; then
  PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
  WRAP=(sandbox-exec -p "$PROFILE")
else
  WRAP=()
fi
env HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS RUST_LOG=info \
  nohup $WRAP $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do
  curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break
  sleep 1
done
echo "S=$S daemon pid $(cat $S/daemon.pid) mock pid $(cat $S/mock.pid)"
grep -c "MCP-external HTTP connect" $S/log/daemon.log | sed 's/^/MCP-external HTTP connect lines: /'
