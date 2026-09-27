# Go Demo — Space App

A minimal Space App written in Go, to copy from when writing a new one.
Equivalents: [Node](../../../senclaw-app-sdk/examples/space-app-node-demo) ·
[Python](../../../senclaw-app-sdk-python/examples/space-app-python-demo).

```bash
# Run by hand (dev)
SENCLAW_SPACE_APP_ID=go-demo PORT=4830 go run .

# Install into a running daemon
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' \
  -d "{\"path\": \"$(pwd)\"}"
```

What the manifest declares:

| Declaration | Meaning |
|---|---|
| `runtime.mode: "session"` | Not started with the daemon. Starts when the app is opened or an agent calls a tool, stops after `idleTimeoutSecs` idle seconds |
| `runtime.runner: "shell"` + `start: "go run ."` | No build step — but the first launch compiles, and the daemon allows 30s to reach the health endpoint. A shipped app uses `runner: "binary"` with a prebuilt program instead |
| `requires.bin: ["go"]` | Checked at install **and** before every launch; a miss refuses the launch with the reason, rather than `exit 127` in a log |
| `sandbox` | Confinement applied at install, without waiting for the user to switch it on in Plugins |

There is no `runtime.install` here on purpose: the daemon runs install commands
for the node and python runners only, so a `go build` declared there would be
skipped without a word. Build in `start`, or ship the binary.

`go.mod` carries a `replace` pointing at the SDK in this repo so the demo builds
from a clone with no network. A real app drops that line and lets `go get` fetch
a tagged version.

See also: [`docs/space-app-lifecycle.md`](../../../../docs/space-app-lifecycle.md).
