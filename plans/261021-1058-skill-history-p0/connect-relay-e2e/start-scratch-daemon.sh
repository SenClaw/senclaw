#!/bin/bash
# Scratch daemon: own HOME / SENCLAW_HOME / cwd / ports, and a kernel guard so a stray call to the real daemon's ports fails.
cd /private/tmp/claude-501/-Users-benji-Projects-SenClaw/acedfca1-36ef-4c11-adf4-211d90a837db/scratchpad/e2e/cwd
PROFILE='(version 1)(allow default)(deny network-outbound (remote tcp "localhost:18788"))(deny network-outbound (remote tcp "localhost:18789"))(deny network-outbound (remote tcp "localhost:18790"))'
exec env -u SENCLAW_BASE_URL -u SENCLAW_APP_TOKEN_MODE HOME=/private/tmp/claude-501/-Users-benji-Projects-SenClaw/acedfca1-36ef-4c11-adf4-211d90a837db/scratchpad/e2e/home SENCLAW_HOME=/private/tmp/claude-501/-Users-benji-Projects-SenClaw/acedfca1-36ef-4c11-adf4-211d90a837db/scratchpad/e2e/home/.senclaw SENCLAW_UI_PORT=38788 SENCLAW_WS_PORT=38789 \
  sandbox-exec -p "$PROFILE" /private/tmp/claude-501/-Users-benji-Projects-SenClaw/acedfca1-36ef-4c11-adf4-211d90a837db/scratchpad/e2e/senclaw-bin start
