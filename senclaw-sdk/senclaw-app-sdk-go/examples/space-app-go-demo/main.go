// A complete Space App in Go, in one file.
//
// What it demonstrates, in the order the daemon exercises it:
//
//  1. `requires.bin: ["go"]` is checked before this program is ever launched —
//     because this demo is started with `go run .` so it needs no build step.
//     A shipped app replaces that with a compiled binary; see README.md.
//  2. There is no install step for a Go app. The daemon runs `runtime.install`
//     for node and python runners only, so whatever `start` names must already
//     be runnable.
//  3. `runtime.mode: "session"` — the daemon does not start this at boot. It
//     starts when the user opens the app, or when an agent calls one of the
//     tools below, and stops it again 60 seconds after the last request.
//  4. Its two MCP tools are in every agent's roster even while it is stopped:
//     the tool list is cached, and the MCP URL points at the daemon's proxy,
//     which starts the app before forwarding the call.
//
// Run it by hand for development:
//
//	SENCLAW_SPACE_APP_ID=go-demo PORT=4830 go run .
package main

import (
	"context"
	"fmt"
	"net/http"
	"runtime"
	"strings"
	"time"

	senclaw "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go"
)

const appID = "go-demo"

var startedAt = time.Now()

func main() {
	// MustNew reads SENCLAW_SPACE_APP_ID and SENCLAW_BASE_URL from the
	// environment the daemon sets. The explicit id keeps a bare `go run .`
	// working during development.
	space := senclaw.MustNew(senclaw.WithAppID(appID))

	mcp := senclaw.NewMCPServer("go-demo-mcp", "1.0.0")

	mcp.Tool("godemo_env", "Report the Go runtime this Space App is running on",
		senclaw.Schema{"type": "object", "properties": senclaw.Schema{}},
		func(context.Context, map[string]any) (any, error) {
			return map[string]any{
				"go":         runtime.Version(),
				"platform":   runtime.GOOS + "/" + runtime.GOARCH,
				"uptimeSecs": int(time.Since(startedAt).Seconds()),
			}, nil
		})

	mcp.Tool("godemo_summarise", "Summarise a piece of text in three sentences",
		senclaw.Schema{
			"type": "object",
			"properties": senclaw.Schema{
				"text": senclaw.Schema{"type": "string", "description": "The text to summarise"},
			},
			"required": []string{"text"},
		},
		func(ctx context.Context, args map[string]any) (any, error) {
			text := strings.TrimSpace(senclaw.String(args, "text"))
			if text == "" {
				// A readable sentence, not a transport error: the agent has to
				// know what to do differently, and a JSON-RPC error tells it
				// nothing.
				return senclaw.ErrorContent("`text` is empty — pass the text to summarise."), nil
			}
			// The app holds no provider key. This goes to the daemon, which
			// uses whichever provider the user configured.
			return space.LLM(ctx, senclaw.LLMRequest{
				Prompt:    "Summarise the following in exactly three sentences:\n\n" + text,
				MaxTokens: 600,
			})
		})

	routes := map[string]http.Handler{
		// runtime.healthPath. The daemon waits on this before it calls the app
		// started, and polls it afterwards, so it must stay cheap and never
		// block.
		"GET /api/status": senclaw.JSONHandler(func(*http.Request) (any, error) {
			return map[string]any{
				"ok": true, "app": appID,
				"uptimeSecs": int(time.Since(startedAt).Seconds()),
			}, nil
		}),

		// The config KV: the same store the app's own UI reads and writes,
		// which is why settings belong here and not in a file an update would
		// overwrite.
		"POST /api/visit": senclaw.JSONHandler(func(r *http.Request) (any, error) {
			var visits int
			if _, err := space.GetConfig(r.Context(), "visits", &visits); err != nil {
				return nil, err
			}
			visits++
			if err := space.SetConfig(r.Context(), "visits", visits); err != nil {
				return nil, err
			}
			return map[string]any{"visits": visits}, nil
		}),
	}

	err := senclaw.Serve(senclaw.Config{
		Routes:     routes,
		HealthPath: "/api/status",
		MCPPath:    "/api/mcp/sse",
		MCP:        mcp,
		StaticDir:  "web",
		// A session app is stopped with SIGTERM and killed two seconds later.
		OnShutdown: func(context.Context) error {
			fmt.Println("[go-demo] flushing before exit")
			return nil
		},
		DefaultPort: 4830,
	})
	if err != nil {
		panic(err)
	}
}
