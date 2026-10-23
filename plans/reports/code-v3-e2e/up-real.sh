#!/bin/zsh
# usage: up-real.sh <senclaw binary> <sen-mlx binary> <model dir> — isolated daemon + a real local model, no mock
set -u
BIN=$1; MLX=$2; MODEL=$3
HERE=$(cd "$(dirname "$0")" && pwd)
S=$HERE/real
UI=48788; WS=48789; LLM=38931
for p in $UI $WS $LLM; do
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/proj $S/log
cp "$BIN" $S/senclaw-bin
cd $S/root && env HOME=$S/home nohup "$MLX" serve --host 127.0.0.1 --port $LLM --model "$MODEL" > $S/log/mlx.log 2>&1 &
echo $! > $S/mlx.pid
for i in {1..120}; do curl -s -m 2 http://127.0.0.1:$LLM/v1/models >/dev/null 2>&1 && break; sleep 2; done
NAME=$(curl -s -m 5 http://127.0.0.1:$LLM/v1/models | python3 -c "import sys,json; print(json.load(sys.stdin)['data'][0]['id'])")
echo "model: $NAME"
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"local","label":"local","provider":"custom","baseURL":"http://127.0.0.1:$LLM/v1","modelName":"$NAME","adapt":"openai","apiKey":"local","maxTokens":4096,"contextLength":32000,"editFormat":"fuzzy"}],
 "activeLlmConfigId":"local","activeQuickLlmConfigId":"local","activeCognitiveLlmConfigId":"local"}
JSON
cd $S/proj && git init -q && git config user.email e2e@example.com && git config user.name e2e
printf 'def add(a, b):\n    return a - b\n' > calc.py
printf 'from calc import add\nassert add(2, 3) == 5, "add(2, 3) should be 5"\nassert add(-1, 1) == 0, "add(-1, 1) should be 0"\nprint("OK")\n' > check.py
git add -A && git commit -q -m init
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
cd $S/root && env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS RUST_LOG=info \
  nohup sandbox-exec -p "$PROFILE" $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break; sleep 1; done
echo "daemon pid $(cat $S/daemon.pid), mlx pid $(cat $S/mlx.pid)"
grep -c "MCP-external HTTP connect" $S/log/daemon.log | sed 's/^/MCP-external HTTP connect lines: /'
