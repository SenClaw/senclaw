# `senclaw-app-sdk-go`

Write a SenClaw Space App in Go. Standard library only — no module downloads
before the first build, and `go build` works on an air-gapped machine.

> Node/TypeScript: [`../senclaw-app-sdk`](../senclaw-app-sdk) ·
> Python: [`../senclaw-app-sdk-python`](../senclaw-app-sdk-python) ·
> Rust: [`../../app-space-sdk`](../../app-space-sdk).
> App lifecycle (background/session), `requires`, `sandbox`:
> [`docs/space-app-lifecycle.md`](../../docs/space-app-lifecycle.md).
> Tiếng Việt: [README-vn.md](README-vn.md).

## Install

```bash
go get github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go
```

```go
import senclaw "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go"
```

## Example app

[`examples/space-app-go-demo/`](examples/space-app-go-demo) — a complete Space
App in one file: 2 MCP tools, a UI page, a health endpoint, SIGTERM handling.
Its manifest declares everything that decides how the daemon runs it
(`runtime.mode`, `runtime.runner`, `requires`, `sandbox`).

```bash
cd examples/space-app-go-demo
SENCLAW_SPACE_APP_ID=go-demo PORT=4830 go run .

# or install it into a running daemon
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' -d "{\"path\": \"$(pwd)\"}"
```

## Read this first: a Go app has no install step

The daemon runs `runtime.install` for the **node** and **python** runners only
([`src/apps/prepare.rs`](../../src/apps/prepare.rs) returns immediately for
`binary` and `shell`). A Go app that declares `"install": "go build -o app ."`
gets that command silently skipped, and then `start` points at a binary nobody
built. `manifest.Validate` flags it; nothing in the daemon will.

Two shapes actually work:

| | `start` | `runner` | Trade-off |
|---|---|---|---|
| **Compiled** (ship this) | `./my-app` | `binary` (inferred from `./`) | Starts in milliseconds. You build and ship a binary per platform. |
| **`go run`** (the demo) | `go run .` | `shell` | No build step, needs `requires.bin: ["go"]`. The first launch compiles, and the daemon gives an app **30 seconds** to answer its health endpoint. |

For a compiled app, build before registering — and cross-compile for whatever
the user runs:

```bash
GOOS=darwin GOARCH=arm64 go build -o my-app . # then register-local
```

## Minimal app

`main.go`:

```go
package main

import (
	"context"
	"net/http"

	senclaw "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go"
)

func main() {
	space := senclaw.MustNew() // reads SENCLAW_SPACE_APP_ID + SENCLAW_BASE_URL
	mcp := senclaw.NewMCPServer("demo-mcp", "1.0.0")

	mcp.Tool("demo_summarise", "Summarise a piece of text", senclaw.Schema{
		"type":       "object",
		"properties": senclaw.Schema{"text": senclaw.Schema{"type": "string"}},
		"required":   []string{"text"},
	}, func(ctx context.Context, args map[string]any) (any, error) {
		// The app NEVER holds a provider API key — every model call goes
		// through the daemon, using the provider the user configured.
		return space.LLM(ctx, senclaw.LLMRequest{
			Prompt:    "Summarise in three sentences:\n\n" + senclaw.String(args, "text"),
			MaxTokens: 800,
		})
	})

	senclaw.Serve(senclaw.Config{
		Routes: map[string]http.Handler{
			"GET /api/status": senclaw.JSONHandler(func(*http.Request) (any, error) {
				return map[string]any{"ok": true}, nil
			}),
		},
		HealthPath:  "/api/status",
		MCPPath:     "/api/mcp/sse",
		MCP:         mcp,
		StaticDir:   "web",
		DefaultPort: 4830,
	})
}
```

`senclaw-manifest.json`:

```json
{
  "id": "demo-go",
  "name": "Demo Go",
  "description": "Space App written in Go",
  "icon": "🐹",
  "runtime": {
    "kind": "server",
    "mode": "session",
    "runner": "binary",
    "start": "./demo-go",
    "healthPath": "/api/status",
    "port": 4830
  },
  "integration": { "type": "iframe", "url": "/" },
  "mcp": {
    "name": "demo-go-mcp",
    "transport": "http",
    "path": "/api/mcp/sse",
    "autoRegister": true
  }
}
```

Install into a running daemon:

```bash
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' -d '{"path": "/path/to/app"}'
```

## What the daemon gives an app

```go
space := senclaw.MustNew(senclaw.WithAppID("my-app"))

space.Capabilities(ctx)                              // what this daemon supports

text, err := space.LLM(ctx, senclaw.LLMRequest{Prompt: p, System: s, MaxTokens: 4000})
reply, err := space.LLMDetailed(ctx, req)            // text, model, finish, usage
out, err := space.Agent(ctx, "do the thing")         // a full agent turn, with tools

space.KnowledgeSave(ctx, senclaw.Memory{Text: "remember this", Space: "proj"})
space.KnowledgeSearch(ctx, "a question", "proj", 10) // raw hits
space.KnowledgeRecall(ctx, senclaw.RecallQuery{Query: "a question", Space: "proj"})

space.GetConfig(ctx, "prefs", &prefs)                // the same KV the app's UI uses
space.SQLiteScan(ctx, &rows, "SELECT * FROM t WHERE a = ?", 1)

active, models, err := space.ListModels(ctx)
space.UsageReport(ctx, senclaw.Usage{Model: m, Provider: p, InputTokens: 100})
```

Three places this goes wrong:

- **`LLM` returns an error when the reply was truncated.** `Finish == "length"`
  means the model hit the `MaxTokens` ceiling mid-sentence, and half an answer
  is indistinguishable from a short one. Use `LLMDetailed` to handle it
  yourself.
- **A failed bridge action still answers HTTP 200**, with
  `{"status":"error"}` in the body. The SDK turns that into an error; if you
  call the bridge by hand, check the `status` field or a dead provider reads as
  an empty string.
- **Use `LLMRequest.Profile`, not `SetActiveModel`.** The active model is
  **global** — the agent and every other app share it.

Errors carry the daemon's own message and status: `senclaw.StatusOf(err)`, or
`errors.As(err, &senclawErr)`.

## Dispatch

Let the daemon's `MCPDispatcher` drive the app:

```go
import "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/dispatch"

type Store struct{ dispatch.Unleased } // no-op Heartbeat + Reclaim

func (s *Store) ClaimReady(ctx context.Context, c dispatch.Capacity) ([]dispatch.WorkItem, error) {
	// must be atomic — an item handed out twice is run twice
}
func (s *Store) Finalize(ctx context.Context, id string, o dispatch.Outcome) error { … }

senclaw.Serve(senclaw.Config{
	Routes: senclaw.MergeRoutes(dispatch.Routes(&Store{}, ""), myRoutes),
})
```

Field names are snake_case (`depends_on`, `timeout_secs`, `item_id`) because the
engine parses them with serde — camelCase is dropped silently, and it surfaces
as a dependency that never held rather than as an error. `WorkItem` marshals
empty slices as `[]` for the same reason: serde's `Vec` rejects an explicit
`null`.

## Serving an LLM

Let the app **become a model**: its models appear in the same picker as OpenAI
and Anthropic, and agent turns route to it over HTTP. Implement `Provider` and
merge its two routes into your own.

```go
import "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/llm"

type Mlx struct{ /* … */ }

func (m *Mlx) Models() []llm.ModelCard {
	// vision is REQUIRED — the daemon sends image blocks or falls back to OCR
	// from it, and a text-only endpoint 400s on an image. Never guess it.
	return []llm.ModelCard{llm.NewModelCard("gemma-4-e2b-it-4bit", 128000, 8192, true)}
}

func (m *Mlx) Chat(ctx context.Context, req llm.ChatRequest, sink *llm.Sink) error {
	sink.Text("hello")                       // visible assistant text
	sink.Send(llm.Reasoning("thinking…"))    // shown separately, echoed back next turn
	sink.Send(llm.ToolCall("id", "get_time", "{}"))
	sink.Send(llm.Usage(12, 3))              // prompt, completion — at most once, at the end
	return nil                               // an error ends the stream early
}

func main() {
	provider := &Mlx{}
	// Cache the model list so the picker shows it while the app is STOPPED — a
	// session app is stopped most of the time, and a model nobody can see is a
	// model nobody starts the app for.
	_ = llm.PublishModels(".", provider.Models())

	senclaw.Serve(senclaw.Config{
		Routes: senclaw.MergeRoutes(llm.Routes(provider), myRoutes),
	})
}
```

The manifest turns the app into a provider — the daemon speaks **OpenAI** to it,
so `adapt` is `"openai"` and no new adapter is needed:

```json
"llm": { "autoRegister": true, "path": "/v1", "adapt": "openai", "displayName": "MLX" }
```

`Routes(provider)` serves `GET /v1/models` and `POST /v1/chat/completions` (both
SSE and non-stream). You emit **semantic** events — `Text`, `Reasoning`,
`ToolCall`, `Usage` — and the SDK renders the exact `chat.completion.chunk` wire
the daemon's OpenAI parser expects. Four things it gets right that a hand-rolled
JSON body gets wrong:

- **Each `ToolCall` streams as one delta at a fresh, incrementing index.** The
  consumer accumulates `function.name`/`arguments` by concatenation keyed on
  index, so a reused index welds `get_weatherget_time` together.
- **`Usage` rides its own chunk** with an empty `choices` array and a top-level
  `usage` — the only place the consumer looks.
- **The stream always ends with `data: [DONE]`, failure included.** A mid-stream
  failure becomes an error chunk (`finish_reason: "error"`), not a silent
  truncation the caller cannot tell from a short answer — the status line already
  went out and cannot become a 5xx.
- **`PublishModels` refuses an empty list** and writes-then-renames, so a failed
  startup never wipes a good cache out of the picker.

Load weights lazily in `Chat`, never at startup: the daemon health-gates a new
app on 30 seconds, and an app that reads gigabytes before it binds its port is
reported as failing to start.

## What the SDK handles for you

| | |
|---|---|
| `BindHost()` | `127.0.0.1` unless `SENCLAW_BIND_HOST` says otherwise. An app has **no authentication of its own** — binding `0.0.0.0` opens its whole REST + MCP surface to the LAN |
| `Port()` | The port the daemon assigns via `PORT` |
| `Serve(...)` | Health + static + REST + MCP on one port, and **SIGTERM handling** |
| `Handler(...)` | The same routing without listening — hand it to `httptest.NewServer` |
| `space.LLM(...)` | A model call through the bridge; an error rather than a truncated string when it hits `MaxTokens` |
| `space.SQLite(...)` | The app's own database, always parameterised |
| `space.GetConfig` / `SetConfig` | The same KV the app's UI reads and writes — not a file in the app directory, which an update overwrites |
| `MCPServer` | JSON-RPC `initialize` / `tools/list` / `tools/call`, no MCP SDK needed; a panicking tool becomes a message, not a dead app |
| `llm.Routes(...)` | The OpenAI `/v1/models` + `/v1/chat/completions` wire from semantic `Text`/`Reasoning`/`ToolCall`/`Usage` events — indexed tool calls, usage chunk, `[DONE]` terminator, all correct |
| Static serving | Path-traversal guard plus an `index.html` fallback so a client-side router works |

## The app's access token

The daemon mints one access token per installed app and puts it in the launched
process's environment as `SENCLAW_TOKEN_ACCESS_APP`. It is this app's *identity*:
a token is bound to one app id, and using it against another is refused. Without
it, any local process that knows an app's id — which is public — could read that
app's settings, query its database and drive its AI bridge.

**Outbound is automatic.** `New`/`MustNew` read the token from the environment
and `Space` sends it (plus `X-SenClaw-Api-Version`) on every daemon call. Nothing
to do. Running the app by hand, pass it explicitly:

```bash
SENCLAW_TOKEN_ACCESS_APP=$(curl -s localhost:18788/api/space/apps/demo/token | jq -r .token) go run .
```

**Inbound is opt-in.** An app's own REST and MCP endpoints have no authentication
of their own: the port is open to every process on the machine. Turn on
`RequireAppToken` and the only caller that gets through is the daemon — its proxy
stamps the token on everything it forwards (the UI iframe, the app's own fetches,
MCP tool calls):

```go
senclaw.Serve(senclaw.Config{
	RequireAppToken: true,
	HealthPath:      "/api/status",        // always exempt
	AuthSkipPaths:   []string{"/ws/*"},    // a browser extension dials this directly
})
```

Two things are never refused: a missing token in the environment (that is a bare
`go run .`, and 401ing the health check would make the app look permanently
down), and the exempt paths above.

`SENCLAW_API_VERSION` carries the contract version — `APIVersion` in this SDK,
currently 2. A daemon serving an older contract still answers; one asked for a
version it does not implement replies **426** rather than half-answering.

Full guide, including `SENCLAW_APP_TOKEN_MODE=strict`:
[docs/space-app-api-token.md](https://github.com/NortonBen/SenClaw/blob/main/docs/space-app-api-token.md).

## SIGTERM — read before writing an app

A **session** app is stopped when it goes idle: the daemon sends `SIGTERM` to
the process group and `SIGKILL` two seconds later. `Serve` installs the handler,
closes the listener and runs `OnShutdown`; do not block in there for more than
about a second and a half.

```go
senclaw.Serve(senclaw.Config{
	OnShutdown: func(ctx context.Context) error { return db.Close() },
})
```

## Checking a manifest

```bash
go run github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/cmd/senclaw-manifest senclaw-manifest.json
```

It catches exactly the silent-failure class: `"mode": "backgroud"` (misspelled →
treated as `session`, so an always-on app quietly stops), `network: "hosts"`
with an empty host list (= no network at all), `autoRegister` with no `path`,
`idleTimeoutSecs` below the 15s floor, and `install` on a runner that never runs
it.

Or from Go:

```go
if problems := manifest.Validate(m); len(problems) > 0 { … }
```

## Tests

```bash
go test ./...
```

They pin the two contracts that fail invisibly: the JSON-RPC methods SenClaw
actually sends, and the exact keys the bridge and the dispatch engine parse.
