#!/bin/zsh
# usage: up-lan.sh <senclaw binary> <base url> <model name> — scratch daemon on a model served on the network (LM Studio),
# project = seed/ (a small Python to-do module with tests) in a fresh git repo. Same isolation as up.sh.
set -u
BIN=$1; BASE=$2; NAME=$3
HERE=$(cd "$(dirname "$0")" && pwd)
S=$HERE/lan
UI=48798; WS=48799
for p in $UI $WS; do
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
curl -s -m 5 "$BASE/models" >/dev/null || { echo "model server $BASE does not answer"; exit 1; }
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/proj $S/log
cp -c "$BIN" $S/senclaw-bin 2>/dev/null || cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"lan","label":"lan","provider":"custom","baseURL":"$BASE","modelName":"$NAME","adapt":"openai","apiKey":"","maxTokens":8192,"contextLength":128000}],
 "activeLlmConfigId":"lan","activeQuickLlmConfigId":"lan","activeCognitiveLlmConfigId":"lan"}
JSON
cp -R $HERE/seed/. $S/proj/
cd $S/proj && git init -q && git config user.email e2e@example.com && git config user.name e2e && git add -A && git commit -q -m baseline
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
cd $S/root && env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS RUST_LOG=info \
  nohup sandbox-exec -p "$PROFILE" $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..90}; do curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break; sleep 1; done
echo "daemon pid $(cat $S/daemon.pid), model $NAME at $BASE"
