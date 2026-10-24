#!/bin/zsh
# usage: up.sh <senclaw binary>  — isolated daemon + scripted mock LLM for the code-v3 e2e
set -u
BIN=$1
HERE=$(cd "$(dirname "$0")" && pwd)
S=$HERE/run
UI=48788; WS=48789; MOCK=48931
for p in $UI $WS $MOCK; do
  if lsof -nP -iTCP:$p -sTCP:LISTEN >/dev/null 2>&1; then echo "port $p busy"; exit 1; fi
done
rm -rf $S; mkdir -p $S/home/.senclaw $S/root $S/proj $S/outside $S/log $S/mock
cp "$BIN" $S/senclaw-bin
cat > $S/home/.senclaw/config.json <<JSON
{"llmConfigs":[{"id":"mock","label":"mock","provider":"custom","baseURL":"http://127.0.0.1:$MOCK/v1","modelName":"mock-model","adapt":"openai","apiKey":"local","maxTokens":2048,"contextLength":32000}],
 "activeLlmConfigId":"mock","activeQuickLlmConfigId":"mock","activeCognitiveLlmConfigId":"mock"}
JSON
# the project: a git repo with its own instructions and a manifest
cd $S/proj && git init -q && git config user.email e2e@example.com && git config user.name e2e
printf 'AGENTS-MARKER: use tabs, never edit generated/.\n' > AGENTS.md
printf '[package]\nname = "e2e"\nversion = "0.1.0"\n' > Cargo.toml
printf 'def hello():\n    return "hi"\n\nprint(hello())\n' > hello.py
git add -A && git commit -q -m init
# a user-authored PreStop hook: refuses to let the agent stop once, then allows it
cat > $S/prestop.sh <<'HOOK'
#!/bin/sh
M="$(dirname "$0")/proj/.prestop-seen"
if [ ! -f "$M" ]; then
  touch "$M"
  echo '{"blocked":true,"reason":"PRESTOP-GATE: the release checklist is not done yet, finish it before you stop"}'
fi
HOOK
chmod +x $S/prestop.sh
cat > $S/home/.senclaw/hooks.json <<JSON
{"hooks":{"PreStop":[{"hooks":[{"type":"command","command":"$S/prestop.sh","blocking":true,"timeout":10}]}]}}
JSON
cd $S/mock && nohup node $HERE/mock-llm.mjs --port $MOCK --out $S/mock > $S/log/mock.log 2>&1 &
echo $! > $S/mock.pid
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
cd $S/root && env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=$UI SENCLAW_WS_PORT=$WS RUST_LOG=info \
  nohup sandbox-exec -p "$PROFILE" $S/senclaw-bin start > $S/log/daemon.log 2>&1 &
echo $! > $S/daemon.pid
for i in {1..60}; do
  curl -s -m 1 http://127.0.0.1:$UI/api/auth/status >/dev/null 2>&1 && break
  sleep 1
done
echo "S=$S daemon pid $(cat $S/daemon.pid) mock pid $(cat $S/mock.pid)"
grep -c "MCP-external HTTP connect" $S/log/daemon.log | sed 's/^/MCP-external HTTP connect lines: /'
