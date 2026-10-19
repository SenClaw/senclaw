#!/bin/zsh
# usage: P5_LLM_BASE=http://<host>:<port>/v1 P5_LLM_MODEL=<id> [P5_LLM_KEY=<key>] env-up.sh <label> <senclaw binary>
# Starts a logging proxy in front of the real model and an isolated daemon (own HOME, SENCLAW_HOME, cwd, ports) whose only
# LLM is that model, reached through the proxy. Never uses 18788/18789/18790; the daemon runs under Seatbelt with loopback
# connects to those ports DENIED. Needs P0_RUNS / P0_UI / P0_WS / P0_MOCK set to free ports (P0_MOCK is the proxy's).
set -u
LABEL=$1; BIN=$2; shift 2
HERE=$(cd "$(dirname "$0")" && pwd)
: "${P5_LLM_BASE:?set P5_LLM_BASE to the OpenAI-compatible base URL, e.g. http://192.168.0.93:1234/v1}"
: "${P5_LLM_MODEL:?set P5_LLM_MODEL to the model id}"
SP=${P0_RUNS:-/tmp/senclaw-p0-runs}
S=$SP/run-$LABEL
UI=${P0_UI:-53788}; WS=${P0_WS:-53789}; PROXY=${P0_MOCK:-53931}
for p in $UI $WS $PROXY; do
  if [ "$p" = 18788 ] || [ "$p" = 18789 ] || [ "$p" = 18790 ]; then echo "refusing the real daemon port $p"; exit 1; fi
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
TARGET=${P5_LLM_BASE%/v1}; TARGET=${TARGET%/}
case "$TARGET" in *:18788|*:18789|*:18790) echo "P5_LLM_BASE is the real daemon"; exit 1;; esac
# The model must answer before anything is started: an unreachable model measures nothing.
if ! curl -s -m 8 -f "$P5_LLM_BASE/models" >/dev/null 2>&1; then echo "GET $P5_LLM_BASE/models did not answer: not measuring"; exit 3; fi
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/log $S/proxy
for d in A; do mkdir -p $S/proj/$d; done
cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"real","label":"real","provider":"custom","baseURL":"http://127.0.0.1:$PROXY/v1","modelName":"$P5_LLM_MODEL","adapt":"openai","apiKey":"${P5_LLM_KEY:-}","maxTokens":8192,"contextLength":128000}],
 "activeLlmConfigId":"real","activeQuickLlmConfigId":"real","activeCognitiveLlmConfigId":"real","thinkingEnabled":true}
JSON
cd $S/proxy && nohup node $HERE/llm-proxy.mjs --target "$TARGET" --port $PROXY --out $S/proxy > $S/log/proxy.log 2>&1 &
echo $! > $S/proxy.pid
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
cd $S/root && env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE -u SENCLAW_HOME \
  HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS \
  SENCLAW_BASE_URL=http://127.0.0.1:$UI SENCLAW_APP_TOKEN_MODE=off SENCLAW_DISPATCH_INTERVAL_SECS=3 \
  RUST_LOG=info "$@" \
  nohup sandbox-exec -p "$PROFILE" $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do
  curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break
  sleep 1
done
echo "S=$S daemon pid $(cat $S/daemon.pid) proxy pid $(cat $S/proxy.pid)"
grep -c "MCP-external HTTP connect" $S/log/daemon.log | sed 's/^/MCP-external HTTP connect lines: /'
