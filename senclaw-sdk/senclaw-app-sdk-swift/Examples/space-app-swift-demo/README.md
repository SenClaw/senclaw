# space-app-swift-demo

A complete SenClaw Space App in Swift, in one file
([`Sources/space-app-swift-demo/main.swift`](Sources/space-app-swift-demo/main.swift)):

- two MCP tools (`swiftdemo_env`, `swiftdemo_uppercase`),
- a model the app serves itself (`swift-echo`, via `llmRoutes`),
- a `/api/status` health endpoint and a static UI page,
- session lifecycle + SIGTERM handling.

## Run it by hand

```bash
SENCLAW_SPACE_APP_ID=swift-demo PORT=4831 swift run
```

Then:

```bash
curl -s localhost:4831/api/status
curl -s localhost:4831/v1/models
curl -sN localhost:4831/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"swift-echo","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

## Install into a running daemon

```bash
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' -d "{\"path\": \"$(pwd)\"}"
```

## Shipping it

The manifest uses `runner: "shell"` + `start: "swift run -c release"`, which
compiles on first launch — fine for development, but the daemon gives an app 30
seconds to answer its health check, so build once by hand first. For a shipped
app, compile a release binary and point `start` at it with `runner: "binary"`:

```bash
swift build -c release      # → .build/release/space-app-swift-demo
```
