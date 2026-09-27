# `senclaw-app-sdk-swift`

Write a SenClaw Space App in Swift. **Foundation only** — no package to resolve
before the first build, and nothing to `pip`/`npm install` before the first
launch.

> Node/TypeScript: [`../senclaw-app-sdk`](../senclaw-app-sdk) ·
> Python: [`../senclaw-app-sdk-python`](../senclaw-app-sdk-python) ·
> Go: [`../senclaw-app-sdk-go`](../senclaw-app-sdk-go) ·
> Rust: [`../../app-space-sdk`](../../app-space-sdk).
> App lifecycle (background/session), `requires`, `sandbox`:
> [`docs/space-app-lifecycle.md`](../../docs/space-app-lifecycle.md).
> Tiếng Việt: [README-vn.md](README-vn.md).

## Install

This package lives in a **subdirectory** of the SenClaw monorepo, and Swift
Package Manager cannot resolve a `url:` dependency that points inside a repo
(unlike Go modules, it wants `Package.swift` at the repo root). So there are two
ways to depend on it:

**In a checkout of this repo** — a path dependency, which is what the example app
uses:

```swift
.package(name: "SenclawSpace", path: "../senclaw-sdk/senclaw-app-sdk-swift"),
// …then, on the target:
.product(name: "SenclawSpace", package: "SenclawSpace"),
```

**Standalone** — the package is mirrored to its own repo (whose root is this
directory) and tagged, so a `url:` dependency works:

```swift
.package(url: "https://github.com/NortonBen/senclaw-app-sdk-swift.git", from: "0.1.0"),
```

```swift
import SenclawSpace
```

Requires macOS 12+ (the server is a small POSIX-socket HTTP server; the client,
manifest, MCP, dispatch and LLM-rendering pieces are pure Foundation).

## Example app

[`Examples/space-app-swift-demo/`](Examples/space-app-swift-demo) — a complete
Space App in one file: 2 MCP tools, a model it serves itself, a UI page, a
health endpoint, SIGTERM handling. Its manifest declares everything that decides
how the daemon runs it (`runtime.mode`, `runtime.runner`, `mcp`, `llm`).

```bash
cd Examples/space-app-swift-demo
SENCLAW_SPACE_APP_ID=swift-demo PORT=4831 swift run

# or install it into a running daemon
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' -d "{\"path\": \"$(pwd)\"}"
```

## Read this first: a Swift app has no install step

The daemon runs `runtime.install` for the **node** and **python** runners only
([`src/apps/prepare.rs`](../../src/apps/prepare.rs) returns immediately for
`binary` and `shell`). A Swift app that declares `"install": "swift build -c
release"` gets that command silently skipped. Two shapes actually work:

| | `start` | `runner` | Trade-off |
|---|---|---|---|
| **Compiled** (ship this) | `./my-app` | `binary` (inferred from `./`) | Starts in milliseconds. You build and ship a binary per platform. |
| **`swift run`** (the demo) | `swift run -c release` | `shell` | No build step, needs `requires.bin: ["swift"]`. **The first launch compiles**, and the daemon gives an app **30 seconds** to answer its health endpoint — build once by hand before the first real launch. |

For a shipped app, build the release binary and point `start` at it:

```bash
swift build -c release           # → .build/release/my-app
```

## Minimal app

`Sources/my-app/main.swift`:

```swift
import Foundation
import SenclawSpace

let space = try SpaceClient(appId: "demo-swift")   // reads SENCLAW_BASE_URL etc.
let mcp = McpServer("demo-swift-mcp")

mcp.tool("demo_summarise", "Summarise a piece of text",
         ["type": "object",
          "properties": ["text": ["type": "string"]],
          "required": ["text"]]) { args in
    // The app NEVER holds a provider API key — every model call goes through the
    // daemon, using the provider the user configured.
    try space.llm(prompt: "Summarise in three sentences:\n\n\((args["text"] as? String) ?? "")",
                  maxTokens: 800)
}

try Serve(Config(
    routes: [RouteKey("GET", "/api/status"): { _ in Response(json: ["ok": true]) }],
    healthPath: "/api/status",
    mcpPath: "/api/mcp/sse",
    mcp: mcp,
    staticDir: "web",
    defaultPort: 4831
))
```

`senclaw-manifest.json`:

```json
{
  "id": "demo-swift",
  "name": "Demo Swift",
  "description": "Space App written in Swift",
  "icon": "🕊️",
  "runtime": {
    "kind": "server",
    "mode": "session",
    "runner": "binary",
    "start": "./demo-swift",
    "healthPath": "/api/status",
    "port": 4831
  },
  "integration": { "type": "iframe", "url": "/" },
  "mcp": {
    "name": "demo-swift-mcp",
    "transport": "http",
    "path": "/api/mcp/sse",
    "autoRegister": true
  }
}
```

## What the daemon gives an app

```swift
let space = try SpaceClient(appId: "my-app")

try space.capabilities()                                  // what this daemon supports

let text = try space.llm(prompt: p, system: s, maxTokens: 4000)
let reply = try space.llmDetailed(prompt: p)              // text, model, finish, usage
let out = try space.agent("do the thing")                 // a full agent turn, with tools

try space.knowledgeSave("remember this", space: "proj")
try space.knowledgeSearch("a question", space: "proj")    // raw hits
try space.knowledgeRecall("a question", space: "proj")    // one synthesised answer

try space.getConfig("prefs")                              // the same KV the app's UI uses
try space.sqlite("SELECT * FROM t WHERE a = ?", [1])

let (active, models) = try space.listModels()
space.usageReport(model: m, provider: p, inputTokens: 100, outputTokens: 50)
```

Three places this goes wrong:

- **`llm` throws when the reply was truncated.** `finish == "length"` means the
  model hit the `maxTokens` ceiling mid-sentence, and half an answer is
  indistinguishable from a short one. Use `llmDetailed` to handle it yourself.
- **A failed bridge action still answers HTTP 200**, with `{"status":"error"}`
  in the body. The SDK turns that into a thrown `SenclawError`; if you call
  `bridge` by hand, check the `status` field or a dead provider reads as an
  empty string.
- **Use `profile:` on `llm`, not `setActiveModel`.** The active model is
  **global** — the agent and every other app share it.

## Serving an LLM

Let the app **become a model**: its models appear in the same picker as OpenAI
and Anthropic, and agent turns route to it over HTTP. Conform to `LlmProvider`
and merge its two routes into your own.

```swift
struct Mlx: LlmProvider {
    func models() -> [ModelCard] {
        // vision is REQUIRED — the daemon sends image blocks or falls back to
        // OCR from it, and a text-only endpoint 400s on an image. Never guess it.
        [ModelCard("gemma-4-e2b-it-4bit", contextLength: 128_000, maxOutputTokens: 8192, vision: true)]
    }

    func chat(_ req: ChatRequest, _ sink: ChunkSink) throws {
        sink.text("hello")                                   // visible assistant text
        sink.send(.reasoning("thinking…"))                   // shown separately, echoed back next turn
        sink.send(.toolCall(id: "id", name: "get_time", arguments: "{}"))
        sink.send(.usage(promptTokens: 12, completionTokens: 3))  // at most once, at the end
    }                                                        // throwing ends the stream early
}

let provider = Mlx()
// Cache the model list so the picker shows it while the app is STOPPED — a
// session app is stopped most of the time, and a model nobody can see is a model
// nobody starts the app for.
try? publishModels(FileManager.default.currentDirectoryPath, provider.models())

try Serve(Config(routes: llmRoutes(provider).merging(myRoutes) { _, b in b }))
```

The manifest turns the app into a provider — the daemon speaks **OpenAI** to it,
so `adapt` is `"openai"` and no new adapter is needed:

```json
"llm": { "autoRegister": true, "path": "/v1", "adapt": "openai", "displayName": "MLX" }
```

`llmRoutes(provider)` serves `GET /v1/models` and `POST /v1/chat/completions`
(both SSE and non-stream). You emit **semantic** events — `.text`, `.reasoning`,
`.toolCall`, `.usage` — and the SDK renders the exact `chat.completion.chunk`
wire the daemon's OpenAI parser expects. Four things it gets right that a
hand-rolled JSON body gets wrong:

- **Each `.toolCall` streams as one delta at a fresh, incrementing index.** The
  consumer accumulates `function.name`/`arguments` by concatenation keyed on
  index, so a reused index welds `get_weatherget_time` together.
- **`.usage` rides its own chunk** with an empty `choices` array and a top-level
  `usage` — the only place the consumer looks.
- **The stream always ends with `data: [DONE]`, failure included.** A mid-stream
  failure becomes an error chunk (`finish_reason: "error"`), not a silent
  truncation the caller cannot tell from a short answer — the status line already
  went out and cannot become a 5xx.
- **`publishModels` refuses an empty list** and writes-then-renames, so a failed
  startup never wipes a good cache out of the picker.

Load weights lazily in `chat`, never at startup: the daemon health-gates a new
app on 30 seconds, and an app that reads gigabytes before it binds its port is
reported as failing to start. `vision` on `ModelCard` is required and comes from
the checkpoint's own `config.json` — a name-based guess is right or wrong by
accident, and the wrong direction is a hard 400 that fails the whole turn.

## What the SDK handles for you

| | |
|---|---|
| `bindHost()` | `127.0.0.1` unless `SENCLAW_BIND_HOST` says otherwise. An app has **no authentication of its own** — binding `0.0.0.0` opens its whole REST + MCP surface to the LAN |
| `appPort()` | The port the daemon assigns via `PORT` |
| `Serve(...)` | Health + static + REST + MCP on one port, and **SIGTERM handling** |
| `space.llm(...)` | A model call through the bridge; throws rather than returning a truncated string when it hits `maxTokens` |
| `space.sqlite(...)` | The app's own database, always parameterised |
| `space.getConfig` / `setConfig` | The same KV the app's UI reads and writes — not a file in the app directory, which an update overwrites |
| `McpServer` | JSON-RPC `initialize` / `tools/list` / `tools/call`, no MCP library needed; a throwing tool becomes a message, not a dead app |
| `llmRoutes(...)` | The OpenAI `/v1/models` + `/v1/chat/completions` wire from semantic `.text`/`.reasoning`/`.toolCall`/`.usage` events — indexed tool calls, usage chunk, `[DONE]` terminator, all correct |
| Static serving | Path-traversal guard plus an `index.html` fallback so a client-side router works |

## Dispatch

Let the daemon's `MCPDispatcher` drive the app — conform to `DispatchProvider`
and merge its four routes into your own:

```swift
struct Store: DispatchProvider {              // heartbeat + reclaim have no-op defaults
    func claimReady(_ c: Capacity) throws -> [WorkItem] {
        // must be atomic — an item handed out twice is run twice
    }
    func finalize(_ id: String, _ outcome: Outcome) throws { … }
}

try Serve(Config(routes: dispatchRoutes(Store()).merging(myRoutes) { _, b in b }))
```

Field names are snake_case on the wire (`depends_on`, `timeout_secs`, `item_id`)
because the engine parses them with serde — a camelCase spelling is dropped
silently, surfacing as a dependency that never held rather than as an error.

## The app's access token

The daemon mints one access token per installed app and puts it in the launched
process's environment as `SENCLAW_TOKEN_ACCESS_APP`. It is this app's *identity*:
a token is bound to one app id, and using it against another is refused. Without
it, any local process that knows an app's id — which is public — could read that
app's settings, query its database and drive its AI bridge.

**Outbound is automatic.** `SpaceClient` reads the token from the environment and
sends it (plus `X-SenClaw-Api-Version`) on every daemon call. Running the app by
hand, pass it explicitly with `SpaceClient(appToken:)`.

**Inbound is opt-in.** An app's own REST and MCP endpoints have no authentication
of their own: the port is open to every process on the machine. Turn on
`requireAppToken` and the only caller that gets through is the daemon — its proxy
stamps the token on everything it forwards (the UI iframe, the app's own fetches,
MCP tool calls):

```swift
try Serve(Config(
    requireAppToken: true,
    healthPath: "/api/status",           // always exempt
    authSkipPaths: ["/ws/*"]             // a browser extension dials this directly
))
```

Two things are never refused: a missing token in the environment (that is a bare
`swift run`, and 401ing the health check would make the app look permanently
down), and the exempt paths above.

`SENCLAW_API_VERSION` carries the contract version — `API_VERSION` in this SDK,
currently 2. A daemon serving an older contract still answers; one asked for a
version it does not implement replies **426** rather than half-answering. Full
guide, including `SENCLAW_APP_TOKEN_MODE=strict`:
[docs/space-app-api-token.md](https://github.com/NortonBen/SenClaw/blob/main/docs/space-app-api-token.md).

## SIGTERM — read before writing an app

A **session** app is stopped when it goes idle: the daemon sends `SIGTERM` to the
process group and `SIGKILL` two seconds later. `Serve` installs the handler, runs
`onShutdown` and stops accepting; do not block in there for more than about a
second and a half.

```swift
try Serve(Config(onShutdown: { db.close() }))
```

## Checking a manifest

```bash
swift run senclaw-manifest senclaw-manifest.json
```

It catches exactly the silent-failure class: `"mode": "backgroud"` (misspelled →
treated as `session`, so an always-on app quietly stops), `network: "hosts"` with
an empty host list (= no network at all), `autoRegister` with no `path`,
`idleTimeoutSecs` below the 15s floor, and an `llm.adapt` the daemon does not
route. Or from Swift: `validateManifest(dict)`.

## Tests

```bash
swift test
```

They pin the two contracts that fail invisibly: the JSON-RPC methods SenClaw
actually sends, and the exact bytes the OpenAI adapter and the dispatch engine
parse — the tool-call index, the usage chunk, the `[DONE]` terminator.
