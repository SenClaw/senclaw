#!/bin/zsh
# usage: up-lan.sh <senclaw binary> <base url> <model name> [seed git repo] — isolated daemon on a model served elsewhere on the network (LM Studio etc.)
# The project is a bare git repo (no manifest), like the folder a person starts a small module in; with a seed repo it starts as that repo's HEAD.
set -u
BIN=$1; BASE=$2; NAME=$3; SEED=${4:-}
EDITFMT=${EDIT_FORMAT:+,\"editFormat\":\"$EDIT_FORMAT\"}   # optional: EDIT_FORMAT=fuzzy|udiff|whole
HERE=$(cd "$(dirname "$0")" && pwd)
S=$HERE/lan
UI=48788; WS=48789
for p in $UI $WS; do
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
curl -s -m 5 "$BASE/models" >/dev/null || { echo "model server $BASE does not answer"; exit 1; }
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/proj $S/log
cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"lan","label":"lan","provider":"custom","baseURL":"$BASE","modelName":"$NAME","adapt":"openai","apiKey":"","maxTokens":8192,"contextLength":128000$EDITFMT}],
 "activeLlmConfigId":"lan","activeQuickLlmConfigId":"lan","activeCognitiveLlmConfigId":"lan"}
JSON
cd $S/proj && git init -q && git config user.email e2e@example.com && git config user.name e2e
if [ -n "$SEED" ]; then git -C "$SEED" archive HEAD | tar -x -C $S/proj && git add -A && git commit -q -m baseline; fi
# the guard denies a stray connect to the real daemon; the LAN stays reachable
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
cd $S/root && env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS RUST_LOG=info \
  nohup sandbox-exec -p "$PROFILE" $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break; sleep 1; done
echo "daemon pid $(cat $S/daemon.pid), model $NAME at $BASE"
