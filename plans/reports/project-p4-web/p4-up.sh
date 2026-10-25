#!/bin/zsh
# usage: P4_RUNS=<dir> p4-up.sh <senclaw binary> [web dist dir]
# Isolated daemon for the web-app project UI run (ports 51788/51789, mock LLM 51931). Mirrors project-p0-spike/env-up.sh, plus the
# Seatbelt guard (a stray call to the real daemon's ports fails) and a base URL for the
# kanban AI routes that points at THIS daemon.
set -u
SC=${P4_RUNS:-/tmp/senclaw-p4-runs}   # where the run directory goes
HERE=/Users/benji/Projects/SenClaw/senclaw/plans/reports/project-p0-spike
BIN=${1:?usage: p4-up.sh <senclaw binary> [web dist dir]}
DIST=${2:-/Users/benji/Projects/SenClaw/.worktrees/web-app-project/dist}
S=$SC/run-p4web
UI=51788; WS=51789; MOCK=51931
for p in $UI $WS $MOCK; do
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
mkdir -p $SC; rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/log $S/mock $S/proj/orion/src $S/proj/billing
cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"mock","label":"mock","provider":"custom","baseURL":"http://127.0.0.1:$MOCK/v1","modelName":"mock-model","adapt":"openai","apiKey":"local","maxTokens":2048,"contextLength":32000}],
 "activeLlmConfigId":"mock","activeQuickLlmConfigId":"mock","activeCognitiveLlmConfigId":"mock"}
JSON
# A real folder to make a project of: a git repository with a CLAUDE.md.
( cd $S/proj/orion && git init -q . && printf '# Orion\nAPI service.\n' > README.md && printf 'Use Rust 2021.\n' > CLAUDE.md && echo 'fn main() {}' > src/main.rs )
echo 'billing notes' > $S/proj/billing/notes.md
cd $S/mock && nohup node $HERE/mock-llm.mjs --port $MOCK --out $S/mock > $S/log/mock.log 2>&1 &
echo $! > $S/mock.pid
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
cd $S/root && env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE \
  HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS \
  SENCLAW_WEB_DIST=$DIST SENCLAW_BASE_URL=http://127.0.0.1:$UI SENCLAW_APP_TOKEN_MODE=off \
  SENCLAW_COGNITIVE_REFLECT_WINDOW_IDLE_MS=6000 RUST_LOG=info \
  nohup sandbox-exec -p "$PROFILE" $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do
  curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break
  sleep 1
done
echo "S=$S daemon pid $(cat $S/daemon.pid) mock pid $(cat $S/mock.pid)"
